//! Agent / frontend boundary.  Workers stamp [`Kind`]s with their
//! [`AgentId`] through an [`Emitter`]; consumers implement [`Sink`].

use crate::cancel;
use crate::card::{Card, IoEvent};
use crate::event::ProviderErrorRecord;
use crate::provider::{Tuning, Usage};
use crate::transcript::Transcript;
use ral_core::Value;
use serde::Serialize;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The identity of an agent node.  Every agent — the trunk and every forked
/// child alike — has one; a child's id *is* its `AgentId`, so the `agents`
/// listing and `agent_cancel` reuse the node identity rather than minting a
/// parallel one.  Opaque: a capability for status and cancellation, not a
/// content hash.
pub type AgentId = u64;

/// When a message in the inbox may be drained into the model's context.
///
/// The boundary is a *per-message* property, not a global rule.  Everything
/// drains at the next tool-call boundary — user steering, a finished agent's
/// result, a scheduled wakeup, a settled `spawn`'s surface batch, a self-nudge
/// — so it reaches the model as soon as the current tool batch settles rather
/// than waiting out the whole turn.  The sole exception is a slash command:
/// it is interpreted against the session (a `/model` swap, a `/clear`) by the
/// drive loop's `Control`, never shown to the model, so it waits for the turn
/// boundary where the session is `ReadyForUser`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Boundary {
    /// May drain mid-turn, at a tool-call boundary — everything but a command.
    Tool,
    /// Drains only at the turn boundary: a slash command, run by `Control`.
    Turn,
}

/// How [`Inbox::next_or_idle`] should treat an empty inbox — the computed
/// `should_park` verdict, re-evaluated on every wake (focus moves under a
/// parked agent, and a schedule can arm or disarm).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParkMode {
    /// A present human is attached — the conversing trunk, or the focused
    /// agent.  Park *and ignore cancellation*: an Esc cancels the current
    /// *turn*, not the agent, which keeps waiting for the next human line.
    Held,
    /// No human, but this agent has live children still running (async
    /// `agent`s it launched).  Each will deliver its result up this agent's
    /// own inbox when it settles, so park — a headless root waiting on its
    /// fleet has a legal "keep still" move rather than being killed at
    /// quiescence.  Like [`Self::UntilCancelled`], a cancellation
    /// (`agent_cancel`, the ceiling) terminates at once, and the wait ends on
    /// its own the moment the last child settles (the next re-evaluation sees
    /// no children and falls through to [`Self::Quiesce`]).
    HeldByChildren,
    /// No human, but a self-schedule is armed and may fire a wakeup.  Park,
    /// but a cancellation (`agent_cancel`, the ceiling) terminates at once —
    /// stop now rather than wait for the schedule.
    UntilCancelled,
    /// Nothing will ever feed this agent again: terminate at quiescence.
    Quiesce,
}

/// How an async agent settled, already reduced to what the parent's
/// synthetic turn needs to say.  The provider-message detail stays in the
/// child's own log; this is the digest delivered through the inbox.
#[derive(Clone, Debug)]
pub enum AgentOutcome {
    /// Finished with a final answer (carried in [`AgentResult::text`]).
    Complete,
    /// Finished but produced no text.
    Empty,
    /// Stopped for a non-routine reason (content filter, step cap, …).
    Stopped(String),
    /// Cancelled — by `agent_cancel`, `/clear`, or the worker ceiling.
    Cancelled,
    /// The run errored (provider error, panic).
    Failed(String),
}

impl AgentOutcome {
    /// The `(body, error)` a `↘` subagent breadcrumb shows: body text on a
    /// completed run, the reason in the header suffix otherwise.  Used by both
    /// the synchronous child's `Kind::SubagentDone` and the async result's
    /// fresh turn, so the two render as the identical dialable block.
    pub fn breadcrumb(&self, text: &str) -> (String, Option<String>) {
        match self {
            AgentOutcome::Complete => (text.to_string(), None),
            AgentOutcome::Empty => (String::new(), None),
            AgentOutcome::Stopped(r) => (String::new(), Some(r.clone())),
            AgentOutcome::Cancelled => (String::new(), Some("cancelled".into())),
            AgentOutcome::Failed(e) => (String::new(), Some(e.clone())),
        }
    }

    /// The synchronous `agent` tool_result text the parent sees in this turn.
    pub fn reply(&self, text: &str) -> String {
        match self {
            AgentOutcome::Complete => text.to_string(),
            AgentOutcome::Empty => "(child returned empty reply)".into(),
            AgentOutcome::Stopped(r) => format!("(child stopped: {r})"),
            AgentOutcome::Cancelled => "(child cancelled)".into(),
            AgentOutcome::Failed(e) => format!("call error: {e}"),
        }
    }

    /// The marked synthetic-turn text the model sees when an async result is
    /// drained, titled with the child's tab label.
    pub fn marked_turn(&self, title: &str, text: &str) -> String {
        match self {
            AgentOutcome::Complete => format!("[agent '{title}' finished]\n{text}"),
            AgentOutcome::Empty => format!("[agent '{title}' finished with no output]"),
            AgentOutcome::Stopped(r) => format!("[agent '{title}' stopped: {r}]"),
            AgentOutcome::Cancelled => format!("[agent '{title}' was cancelled]"),
            AgentOutcome::Failed(e) => format!("[agent '{title}' failed: {e}]"),
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
    pub title: String,
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
    /// The marked synthetic-turn text the model sees when this is drained.
    fn render(&self) -> String {
        self.outcome.marked_turn(&self.title, &self.text)
    }
}

/// A live agent sent a peer message to another live agent.
#[derive(Clone, Debug)]
pub struct AgentMessage {
    pub from: AgentId,
    pub from_title: String,
    pub text: String,
}

impl AgentMessage {
    /// The marked synthetic-turn text the recipient model sees when this
    /// message drains.
    fn render(&self) -> String {
        format!(
            "[EXARCH AGENT {} MESSAGE: {}]\n{}\n[/EXARCH]",
            self.from, self.from_title, self.text
        )
    }
}

/// One typed message waiting in a session's [`Inbox`].
///
/// This is the inbound twin of the outbound [`Kind`] event stream: where
/// the old prompt queue held bare user `String`s, the inbox holds every
/// producer's message, each carrying its *source* (the variant itself) and
/// its *drain boundary* ([`InboxMsg::boundary`]).  A cancellation is
/// deliberately **not** a variant: the control plane (cancel a scope) and
/// the data plane (deliver a message) ride separate rails, so a
/// cancellation is unconstructable here by type.
#[derive(Clone, Debug)]
pub enum InboxMsg {
    /// The user typed a prompt while a turn was running.  Drains at the
    /// tool boundary, except a slash command, which waits for the turn
    /// boundary so the REPL command path interprets it.
    UserSteering(String),
    /// A scheduled wakeup fired (cron / after).  Drains at the tool boundary
    /// as a *marked* injection, so it reaches the model as soon as the current
    /// tool batch settles.
    ScheduledWakeup {
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
    },
    /// An async agent settled.  Drains at the tool boundary as a dialable `↘`
    /// subagent block, so its result reaches the model as soon as the current
    /// tool batch settles.
    AgentResult(AgentResult),
    /// A live peer agent sent a message.  Drains at the tool boundary as a
    /// marked injection, never as human text.
    AgentMessage(AgentMessage),
    /// A synthetic continuation the agent posted to *itself* after an attempt
    /// the nudge registry decided to retry (an empty turn, an early stop, a
    /// budget-free completion gate).  Carries the synthetic user message; it
    /// is the same turn continuing, so it resets no turn latch and renders
    /// with no human chrome.  Self-pushed through the agent's own
    /// [`Mailbox`], never across agents.
    Nudge(String),
    /// A session-affecting slash command (`/clear`, `/model`, `/compact`,
    /// `/quit`) the frontend posted at the turn boundary.  The drive loop —
    /// which owns the session the command mutates — hands it to its
    /// [`Control`](crate::agent::Control); view-only commands (`/help`,
    /// `/copy`, …) are handled frontend-side and never reach here.  Carries
    /// the raw command line.
    Command(String),
    /// A detached `spawn` worker flushed its deferred `surface` batch at
    /// completion — the un-awaited delivery path.  The batch is ordinary
    /// surface vocabulary (io maps, `` `card `` variants) terminated by a
    /// `` `done `` event; the boundary sink posts it here, stamped with the
    /// *root* session id so its cards render in the root viewport (a spawn
    /// worker registers no tab of its own).  Drains at the tool boundary, like
    /// a wakeup or an agent result.  `joined` is the worker's
    /// deliver-once latch, shared with the eliminators (`await`/`race`): the
    /// drain renders this batch only if it wins the test-and-set on the flag,
    /// so a replay that already rendered the cards in-turn suppresses it.
    Surface {
        id: AgentId,
        values: Vec<Value>,
        joined: Arc<Mutex<bool>>,
    },
}

impl InboxMsg {
    /// Where this message may be drained.  Only a slash command waits for the
    /// turn boundary (the `Control` interprets it against the session); every
    /// other message — content or a self-nudge continuation — drains at the
    /// next tool boundary.
    pub fn boundary(&self) -> Boundary {
        match self {
            InboxMsg::Command(_) => Boundary::Turn,
            InboxMsg::UserSteering(s) if s.trim_start().starts_with('/') => Boundary::Turn,
            _ => Boundary::Tool,
        }
    }

    /// The side effect of draining this message into context.  A scheduled
    /// wakeup clears its pending flag here, re-opening its schedule for the
    /// next occurrence — the overlap-skip holds only until the wakeup is
    /// taken.  Other messages have none.
    fn on_drain(&self) {
        if let InboxMsg::ScheduledWakeup { pending, .. } = self {
            pending.store(false, Ordering::Release);
        }
    }
}

/// The model-facing notice [`Turn::Surface`] delivers when a detached `spawn`
/// worker settles un-awaited: which spawn finished, how it settled, and where
/// its output now lives.  This is the "host notifies, don't poll" wake — terse
/// and in the register the model already sees from a subagent breadcrumb.  The
/// worker's surfaced cards have already reached the rail through the
/// `commit_turn` decode; the value record (a return value, captured bytes) is
/// pulled on demand with `await $h`.
fn surface_notice(values: &[Value]) -> String {
    use crate::card::DoneOutcome;
    let settled = match values.iter().rev().find_map(crate::card::value_to_done) {
        Some(DoneOutcome::Ok) => "finished (exit 0)".to_string(),
        Some(DoneOutcome::Err { message, status }) => {
            format!("finished (exit {status}): {message}")
        }
        Some(DoneOutcome::Panic { message }) => format!("panicked: {message}"),
        None => "finished".to_string(),
    };
    format!("Background block {settled}. Await its handle for the value.")
}

/// The next deliverable a turn-boundary drain yields, carrying both the
/// model-facing text *and* its source.
///
/// `drain_turn` once collapsed every source to a bare `String`, so the
/// driver could not tell a human prompt from a wakeup from an agent reply
/// and rendered all three as the human's own prompt-echo.  Threading the
/// source through lets each turn render in its honest medium — a human
/// prompt echoes as the user's turn, a wakeup as marked chrome, an agent
/// reply as the same `↘` block a synchronous child gets — while the model
/// still receives [`Self::text`] unchanged.
#[derive(Clone, Debug)]
pub enum Turn {
    /// A coalesced run of human prompts (the old whole-queue join, so a lone
    /// `/clear` still reaches the command path).  Verbatim model text.
    Human(String),
    /// A scheduled wakeup fired — a fresh, marked turn.  Its prompt may
    /// itself be a slash command, so it still flows through the command
    /// path; when it is not, it renders as marked chrome, not a prompt-echo.
    Wakeup(String),
    /// An async agent settled — rendered as a dialable `↘` subagent block.
    Agent(AgentResult),
    /// A peer agent sent this agent a marked message.
    Message(AgentMessage),
    /// A synthetic nudge continuation the agent posted to itself.  Renders
    /// with no human chrome and, crucially, does **not** reset the turn
    /// latches — it is the same turn continuing.
    Nudge(String),
    /// A session-affecting slash command for the drive loop's [`Control`]
    /// (`/clear`, `/model`, `/compact`, `/quit`).  Carries the raw line.
    ///
    /// [`Control`]: crate::agent::Control
    Command(String),
    /// A detached `spawn` worker flushed its deferred `surface` batch at
    /// completion.  The `commit_turn` arm decodes `values` with the shared
    /// surface decoder and feeds the resulting cards/io into the *root*
    /// viewport (the carried `id`) exactly as a live tool turn would; the
    /// model is woken with [`Self::text`]'s notice.
    Surface { id: AgentId, values: Vec<Value> },
}

impl Turn {
    /// The text the model sees when this turn is drained into context —
    /// unchanged from what each source always rendered.  A `Surface` is the
    /// host's "your spawn settled" notice — it does not re-narrate the cards
    /// (those rendered on the rail), only names the spawn, its outcome, and
    /// that `await` yields its value.
    pub fn text(&self) -> String {
        match self {
            Turn::Human(s) | Turn::Wakeup(s) | Turn::Nudge(s) | Turn::Command(s) => s.clone(),
            Turn::Agent(r) => r.render(),
            Turn::Message(m) => m.render(),
            Turn::Surface { values, .. } => surface_notice(values),
        }
    }

    /// Whether draining this turn resets the per-turn nudge latches and (on
    /// the root path) re-mints the cancellation token.  A [`Turn::Nudge`] is
    /// the same turn continuing, so it resets nothing; every other source is
    /// a genuine turn boundary.
    pub fn resets_turn(&self) -> bool {
        !matches!(self, Turn::Nudge(_))
    }
}

/// The queue an [`Inbox`] consumer and its [`Mailbox`] senders share: a
/// [`VecDeque`] of [`InboxMsg`] under a `Mutex`, plus a [`Condvar`] a parked
/// `next_or_idle` waits on so a push wakes it without polling.
struct Shared {
    queue: Mutex<VecDeque<InboxMsg>>,
    signal: Condvar,
    /// True while the consumer is parked in [`ParkMode::Held`] on an empty
    /// queue: the human-facing yield point.  A producer clears it before
    /// waking the consumer, so frontends can distinguish "prompt is editable"
    /// from "the root is still working" without minting a presentation event.
    waiting_for_input: AtomicBool,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Mutex::new(VecDeque::new()),
            signal: Condvar::new(),
            waiting_for_input: AtomicBool::new(true),
        })
    }
}

/// How long a parked [`Inbox::next_or_idle`] sleeps between condvar wakes
/// before re-checking its cancellation token.  A push notifies the condvar
/// immediately; this bound only governs how fast a cancel (which does not
/// notify) is observed by a parked agent.
const PARK_POLL: Duration = Duration::from_millis(100);

/// The cloneable **sender** side of a session's inbox.  Producers hold a
/// `Mailbox`, never the [`Inbox`]: a schedule re-arms through its own
/// session's `Mailbox`, a finishing child posts its one result through its
/// parent's `Mailbox` ([`Agent::outbox`](crate::agent::Agent)), a
/// `spawn` worker flushes its surface batch through the owning session's
/// `Mailbox`.  The registry holds each peer's `Mailbox` so the frontend can
/// steer a focused tab and the `message` tool can deliver a marked note
/// between live agents without exposing raw senders to model code.
#[derive(Clone)]
pub struct Mailbox {
    shared: Arc<Shared>,
}

impl Mailbox {
    /// Post any message (cron wakeup, agent result, self-nudge, …) and wake a
    /// parked consumer.
    pub fn push(&self, msg: InboxMsg) {
        self.shared
            .waiting_for_input
            .store(false, Ordering::Release);
        self.shared
            .queue
            .lock()
            .expect("inbox lock poisoned")
            .push_back(msg);
        self.shared.signal.notify_all();
    }

    /// Post a user-typed steering prompt — the TUI `Enter`-while-busy path.
    pub fn push_user(&self, prompt: String) {
        self.push(InboxMsg::UserSteering(prompt));
    }

    /// Wake a parked consumer without enqueuing a message, so it re-evaluates
    /// its park verdict — the `TAB` focus-change signal the frontend sends
    /// through the registry's mailboxes.
    pub fn wake(&self) {
        self.shared.signal.notify_all();
    }
}

/// A session's inbox: the owned **consumer** of the typed, multi-producer
/// queue the agent's drive loop pulls its next turn from.  Senders are minted
/// with [`Self::mailbox`].
///
/// The drive loop drains tool-boundary messages mid-turn ([`Self::drain_tool`],
/// from `apply`) and turn-boundary deliverables at the boundary
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
    /// to `self.mailbox().push(msg)`.
    pub fn push(&self, msg: InboxMsg) {
        self.shared
            .waiting_for_input
            .store(false, Ordering::Release);
        self.shared
            .queue
            .lock()
            .expect("inbox lock poisoned")
            .push_back(msg);
        self.shared.signal.notify_all();
    }

    /// Post a user-typed steering prompt through the consumer handle.
    pub fn push_user(&self, prompt: String) {
        self.push(InboxMsg::UserSteering(prompt));
    }

    pub fn is_empty(&self) -> bool {
        self.shared
            .queue
            .lock()
            .expect("inbox lock poisoned")
            .is_empty()
    }

    /// Whether the consumer is parked at a human-input boundary.  The TUI uses
    /// the root inbox's bit to drive the terminal tab title: spinner while the
    /// root has not yielded, idle block once it is genuinely waiting for input.
    pub fn waiting_for_input(&self) -> bool {
        self.shared.waiting_for_input.load(Ordering::Acquire)
    }

    /// Pull every pending user prompt back out for editing at once — all the
    /// `UserSteering` messages in the queue, wherever they sit, leaving any
    /// non-user deliveries (a wakeup, an agent result, a `spawn`'s surface) in
    /// place for the turn boundary.  A user prompt queued behind a wakeup is
    /// still the user's draft and should come back with the rest; the wakeup is
    /// not the user's draft and stays queued.
    ///
    /// Returns oldest-first, or `None` if no user prompts are queued.
    pub fn pop_back_user_all(&self) -> Option<Vec<String>> {
        let mut q = self.shared.queue.lock().expect("inbox lock poisoned");
        let mut prompts: Vec<String> = Vec::new();
        let mut kept: VecDeque<InboxMsg> = VecDeque::with_capacity(q.len());
        while let Some(msg) = q.pop_front() {
            match msg {
                InboxMsg::UserSteering(s) => prompts.push(s),
                other => kept.push_back(other),
            }
        }
        *q = kept;
        (!prompts.is_empty()).then_some(prompts)
    }

    /// Mid-turn drain at a tool-call boundary: take the leading run of
    /// tool-boundary messages — every source but a slash command — and deliver
    /// them, in order, each tagged with its source so the driver renders it in
    /// its honest medium (a `↘` subagent block for an agent result, a marked
    /// wakeup, the cards of a settled `spawn`).  A consecutive run of user
    /// steering coalesces into one [`Turn::Human`].
    ///
    /// The scan stops at the first slash command: it is the only turn-boundary
    /// message ([`Boundary::Turn`]), and it must run against a `ReadyForUser`
    /// session, so it and everything queued behind it stay for the turn
    /// boundary ([`Self::next_or_idle`]).  This is also what holds the human's
    /// own ordering — a `/model` then a prompt swaps before the prompt runs —
    /// since steering queued behind the command is left with it.
    pub fn drain_tool(&self) -> Vec<Turn> {
        let mut q = self.shared.queue.lock().expect("inbox lock poisoned");
        let mut turns = Vec::new();
        while q.front().is_some_and(|m| m.boundary() == Boundary::Tool) {
            if matches!(q.front(), Some(InboxMsg::UserSteering(_))) {
                // Coalesce the consecutive run of (non-slash) steering, as the
                // turn-boundary drain does, so it lands as one human turn.
                let mut text = String::new();
                while let Some(InboxMsg::UserSteering(s)) = q.front() {
                    if s.trim_start().starts_with('/') {
                        break;
                    }
                    let s = match q.pop_front() {
                        Some(InboxMsg::UserSteering(s)) => s,
                        _ => unreachable!("front just checked to be user steering"),
                    };
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(&s);
                }
                turns.push(Turn::Human(text));
            } else {
                let msg = q.pop_front().expect("front present and tool-boundary");
                if let Some(turn) = to_turn(msg) {
                    turns.push(turn);
                }
            }
        }
        turns
    }

    /// Turn-boundary drain: the next deliverable, tagged with its source, or
    /// `None` if the queue is empty.  Never blocks — see [`Self::next_or_idle`]
    /// for the parking variant the drive loop uses.
    pub fn drain_turn(&self) -> Option<Turn> {
        let mut q = self.shared.queue.lock().expect("inbox lock poisoned");
        pop_turn(&mut q)
    }

    /// The drive loop's turn-boundary pull.  Returns the next deliverable; on
    /// an empty queue the `park` verdict — recomputed on every wake — decides
    /// whether to park or terminate ([`ParkMode`]):
    ///
    /// - [`ParkMode::Held`] — a human is attached (the conversing trunk or the
    ///   focused agent).  Parks, ignoring cancellation: an Esc cancels the
    ///   current *turn*, not the agent.
    /// - [`ParkMode::HeldByChildren`] — live children may still deliver a
    ///   result up this inbox.  Parks like [`ParkMode::UntilCancelled`]; a
    ///   cancellation terminates at once, and it falls through to `Quiesce`
    ///   once the last child settles.
    /// - [`ParkMode::UntilCancelled`] — a self-schedule may fire.  Parks, but a
    ///   cancellation terminates at once.
    /// - [`ParkMode::Quiesce`] — nothing will feed this agent again: returns
    ///   `None`, so a de-focused, unscheduled agent (and a headless trunk)
    ///   terminates at quiescence.
    ///
    /// `park` is re-evaluated each iteration, so a `TAB` that de-focuses a
    /// parked agent (which [`wake`](Self::wake)s its inbox) flips `Held` to
    /// `Quiesce` and the agent terminates.  A push wakes the park at once
    /// through the condvar; a cancellation does not notify, so a non-`Held`
    /// park re-checks `cancel` every [`PARK_POLL`].
    pub fn next_or_idle(
        &self,
        park: impl Fn() -> ParkMode,
        cancel: &cancel::Token,
    ) -> Option<Turn> {
        let mut q = self.shared.queue.lock().expect("inbox lock poisoned");
        loop {
            let mode = park();
            // A non-`Held` park (schedule-only, or quiescent) terminates the
            // instant the token trips — `agent_cancel`/ceiling means stop now,
            // dropping any queued messages.  A `Held` park ignores it: the
            // human is present, and an Esc cancels a turn, not the agent.
            if mode != ParkMode::Held && cancel.is_cancelled() {
                return None;
            }
            if let Some(turn) = pop_turn(&mut q) {
                self.shared
                    .waiting_for_input
                    .store(false, Ordering::Release);
                return Some(turn);
            }
            if mode == ParkMode::Quiesce {
                self.shared
                    .waiting_for_input
                    .store(false, Ordering::Release);
                return None;
            }
            self.shared
                .waiting_for_input
                .store(mode == ParkMode::Held, Ordering::Release);
            let (guard, _timeout) = self
                .shared
                .signal
                .wait_timeout(q, PARK_POLL)
                .expect("inbox lock poisoned");
            q = guard;
        }
    }

    /// Wake a parked [`next_or_idle`](Self::next_or_idle) without enqueuing a
    /// message, so it re-evaluates its `park` verdict.  The frontend calls it
    /// on a `TAB` focus change, on both the de-focused and newly-focused
    /// agents, so a de-focused idle agent observes the change and reaps.
    pub fn wake(&self) {
        self.shared.signal.notify_all();
    }

    /// Drop every pending message — `/clear` rebuilds the agent, so neither
    /// queued user prompts nor stale non-human deliveries carry across.
    pub fn clear(&self) {
        self.shared
            .queue
            .lock()
            .expect("inbox lock poisoned")
            .clear();
    }
}

/// Pop the next turn-boundary deliverable from a locked queue, tagged with its
/// source.  A leading run of *user* steering coalesces into one [`Turn::Human`]
/// (preserving the whole-queue join, so a lone `/clear` still reaches the
/// command path); every other source is delivered on its own so the drive loop
/// can render each in its honest medium.  A `Surface` whose deliver-once latch
/// is already set is dropped and the scan continues (a suppressed batch never
/// short-circuits a later deliverable).
fn pop_turn(q: &mut VecDeque<InboxMsg>) -> Option<Turn> {
    loop {
        match q.front()? {
            InboxMsg::UserSteering(_) => {
                let mut text = String::new();
                while matches!(q.front(), Some(InboxMsg::UserSteering(_))) {
                    let s = match q.pop_front() {
                        Some(InboxMsg::UserSteering(s)) => s,
                        _ => unreachable!("front just checked to be user steering"),
                    };
                    if !text.is_empty() {
                        text.push_str("\n\n");
                    }
                    text.push_str(&s);
                }
                return Some(Turn::Human(text));
            }
            _ => {
                let msg = q.pop_front().expect("front checked present");
                match to_turn(msg) {
                    Some(turn) => return Some(turn),
                    // A suppressed `Surface` yields nothing; try the next
                    // message rather than return an empty turn.
                    None => continue,
                }
            }
        }
    }
}

/// Convert one non-user-steering message into the [`Turn`] it delivers,
/// running its drain side effect ([`InboxMsg::on_drain`]).  Shared by the
/// tool-boundary drain ([`Inbox::drain_tool`]) and the turn-boundary drain
/// ([`pop_turn`]).  Yields `None` only for a `Surface` an eliminator already
/// delivered (its deliver-once latch is set): the caller drops it.
fn to_turn(msg: InboxMsg) -> Option<Turn> {
    msg.on_drain();
    Some(match msg {
        InboxMsg::ScheduledWakeup {
            label,
            trigger,
            prompt,
            ..
        } => Turn::Wakeup(format!("[scheduled '{label}' · {trigger}] {prompt}")),
        InboxMsg::AgentResult(r) => Turn::Agent(r),
        InboxMsg::AgentMessage(m) => Turn::Message(m),
        InboxMsg::Nudge(s) => Turn::Nudge(s),
        InboxMsg::Command(s) => Turn::Command(s),
        InboxMsg::Surface { id, values, joined } => {
            let mut won = joined.lock().expect("surface joined latch poisoned");
            if *won {
                return None;
            }
            *won = true;
            Turn::Surface { id, values }
        }
        InboxMsg::UserSteering(_) => {
            unreachable!("user steering coalesced by the caller")
        }
    })
}

pub struct Event {
    pub id: AgentId,
    pub kind: Kind,
}

/// Prefix of the `Kind::Error` message [`pump`] emits when the worker thread
/// unwinds.  Shared so a sink can recognise a recovered panic without
/// matching on free text (the headless result reports it as an error rather
/// than a clean completion).
pub const WORKER_PANIC_PREFIX: &str = "worker panicked: ";

pub enum Kind {
    Born {
        log_dir: PathBuf,
        /// Short human-readable label for this agent, chosen by the
        /// dispatching agent (ASCII alnum / `-` / `_`, 1–24 chars).
        /// Falls back to `sub-{N}` when omitted or invalid.  The TUI
        /// surfaces it in the tab bar; headless ignores it.
        title: String,
        /// The spawning agent's id — the tab's parent.  The TUI records it so
        /// that when a focused agent ends (`reply`), focus falls back to its
        /// parent, recursing toward the trunk.
        parent: AgentId,
    },
    Died,
    Token(String),
    /// A live reasoning token, streamed during the model's thinking phase.
    /// Accumulated by the frontend into a provisional deliberation seat until
    /// the final `Reasoning` event commits a real thinking block.
    Thinking(String),
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
        n: u32,
        tuning: Tuning,
    },
    /// A transient label for the worker's current synchronous phase —
    /// "awaiting model", "compacting".  Emitted before a long op so
    /// the frontend can paint a progress label alongside the spinner —
    /// can name what the worker is doing during an otherwise silent gap:
    /// the user sees what the worker is doing during a silent gap,
    /// bare dot, and the user can see Esc was not swallowed), and the
    /// headless `events.json` keeps it for post-mortem.  Superseded by
    /// the next event of any kind.
    Phase(String),
    ToolCall {
        tool: &'static str,
        cmd: String,
        /// Short, single-line label the sink shows on the rail — the
        /// `ral` tool's mandatory `description`, the `agent` tool's
        /// `title` — with `cmd` revealed when the user opens the call.
        /// `None` means there is nothing to reveal, so the call renders
        /// statically from `cmd` (e.g. `fff`, whose `query` is already
        /// short, and the invalid-input rail header).
        summary: Option<String>,
    },
    ToolResult(String),
    UserPromptEcho(String),
    StopReason(String),
    Error(String),
    /// An operational note the agent's driver issued — a truncation recovery,
    /// a compaction step.  Names *what happened*, like every other operational
    /// `Kind`; the display renders it dim, but *dimness is the renderer's
    /// choice*, not a fact in the vocabulary.  The trace records it as
    /// `system_note`; it has no `events.json` twin, since it is not a message
    /// the model saw.
    SystemNote(String),
    /// A recovery nudge the driver issued between attempts.  The trace records
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
    /// the child's own `Kind::Died` and *before* the spawn rejoins the
    /// parent's tool result.  The event's session id is the parent
    /// (typically root); the TUI lands the breadcrumb in root's
    /// scrollback regardless of nesting depth, since subagent output
    /// otherwise lives only in its own tab and ages out at `LINGER`.
    SubagentDone {
        title: String,
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
    /// model-authored dual of the matrix.  `surface `` `pin [key, body] ``
    /// decodes here, writing `card` to slot `key` and **overwriting in place**
    /// on re-pin.  Unlike [`Kind::Card`], a pin is neither logged nor landed in
    /// scrollback: it is what is *currently true*, not a thing that happened, so
    /// it is rendered ambiently in the reserved register column and updated
    /// where it sits.
    Pin {
        key: String,
        card: Card,
    },
    /// Drop a pinned register slot: `surface `` `unpin [key] ``, or a `` `pin ``
    /// whose body is absent or empty.  A finished plan clears its gauge.
    Unpin {
        key: String,
    },
}

/// One grouped hunk of a whole-file diff, carried by a
/// [`crate::card::Mark::Diff`]: a flat unified list of [`Row`]s — context,
/// deletions, and insertions interleaved exactly as `similar`'s grouped ops
/// yield them.  `start` is the 1-indexed original line of the hunk's first
/// row; the sink walks the rows from there, advancing an old- and a
/// new-side counter — a `Context` advances both, a `Del` advances the old
/// counter (and keeps its pre-edit number), an `Add` advances the new
/// counter (and takes its post-edit number).
#[derive(Clone, Debug, Serialize)]
pub struct Hunk {
    pub start: u32,
    pub rows: Vec<Row>,
}

/// One run of a diff row's text: a contiguous slice flagged `emph` when it is
/// the part that actually changed against the row's paired line — the
/// intra-line word diff `similar` computes.  A context row, and the unchanged
/// stretches that surround a change on a del/add row, carry `emph: false`.
#[derive(Clone, Debug, Serialize)]
pub struct Seg {
    pub emph: bool,
    pub text: String,
}

impl Seg {
    /// A whole, unemphasised run — the shape a context row carries and the
    /// default a plainly-constructed del/add row falls back to.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            emph: false,
            text: text.into(),
        }
    }
}

/// One row of a [`Hunk`]'s unified line list: unchanged context, a removed
/// line, or an inserted line.  Each carries its text as a run of [`Seg`]ments
/// so a del/add can mark the words that changed against its paired line; a
/// context row is a single unemphasised segment.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "tag", content = "segs", rename_all = "snake_case")]
pub enum Row {
    Context(Vec<Seg>),
    Del(Vec<Seg>),
    Add(Vec<Seg>),
}

impl Row {
    /// The row's segments, whatever its kind.
    pub fn segs(&self) -> &[Seg] {
        match self {
            Row::Context(s) | Row::Del(s) | Row::Add(s) => s,
        }
    }

    /// The row's full text — its segments concatenated, dropping the
    /// inline-emphasis distinction (the plain-text/headless rendering).
    pub fn text(&self) -> String {
        self.segs().iter().map(|s| s.text.as_str()).collect()
    }
}

/// A run-scoped usage accumulator.  Where a [`Transcript`] is **per-session**
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
        *self.0.lock().expect("usage meter poisoned") += u;
    }

    /// The run total so far.
    pub fn total(&self) -> Usage {
        *self.0.lock().expect("usage meter poisoned")
    }
}

#[derive(Clone)]
pub struct Emitter {
    tx: Sender<Event>,
    id: AgentId,
    /// The **owning** session's mailbox — root's for the root emitter, the
    /// child's own for a child emitter.  Used by the `spawn` boundary sink to
    /// post a deferred surface batch into the agent that ran the spawn.  An
    /// emitter never carries another agent's mailbox.
    mailbox: Mailbox,
    /// Whether this emitter's channel outlives the spawning turn, so a
    /// *detached* worker (an async `agent` child) may clone it for a live
    /// tab.  The TUI's session-lived bus sets it; headless's per-turn bus
    /// leaves it `false`, keeping async children muted *on the display* —
    /// bus lifetime is a TUI property, not a core obligation.  It does not
    /// gate [`Self::transcript`]: a muted child still records its own trace.
    session_lived: bool,
    /// This emitter's owning session's [`Transcript`].  Every [`Self::emit`]
    /// tees here, so the session's operational trace is written at the emit
    /// seam — independent of who drains the live bus for display, and so a
    /// child muted off a per-turn bus still records its full trace.
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
    pub fn new(tx: Sender<Event>, id: AgentId) -> Self {
        Self::with_mailbox(tx, id, Inbox::new().mailbox())
    }

    pub fn with_mailbox(tx: Sender<Event>, id: AgentId, mailbox: Mailbox) -> Self {
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
}

/// The event channel and its inbox, owned for as long as the host wants a
/// worker→frontend bus to live.  Two lifetimes, one type:
///
/// - [`Self::session`] — minted once at REPL start and held for the whole
///   session.  Each turn's foreground worker and every detached async child
///   clone its sender (session-lived, so a background child gets a live tab);
///   the idle wait drains it as a third source.
/// - [`Self::per_turn`] — minted fresh for one turn (headless, tests), so the
///   channel closes when the turn's worker finishes.  Its emitters are *not*
///   session-lived: an async child stays muted *on the display* (it never
///   streams to a live tab) — the observable display behaviour headless has
///   always had.  It still records its own `transcript.jsonl`, since recording
///   rides the emitter, not the channel's lifetime.
///
/// Either way [`pump_on`] borrows the channel — completion is the per-turn
/// `done` flag, never the channel's lifetime.
pub struct FleetBus {
    tx: Sender<Event>,
    rx: Receiver<Event>,
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
    pub fn session(inbox: Inbox) -> Self {
        Self::build(inbox.mailbox(), true)
    }

    /// A per-turn bus over `inbox` (headless / tests).  Emitters are not
    /// session-lived, so async children stay muted on the display (they still
    /// record their own trace).
    pub fn per_turn(inbox: Inbox) -> Self {
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

    /// The receiver the turn's [`Sink`] drains.
    pub(crate) fn rx(&self) -> &Receiver<Event> {
        &self.rx
    }

    /// An [`Emitter`] stamped with `id`, sharing this bus's sender, root
    /// mailbox, session-lived flag, and run [`UsageMeter`].  The root drive
    /// worker takes one; a child emitter is derived with [`Emitter::child`] /
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
/// flag while no event is arriving.  Small enough that a turn returns
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
/// decides when a turn's event loop ends, shared by the headless default
/// [`Sink::drive`] and the TUI's `drive_events`.
///
/// Drains up to `max` available events through `handle`, then reports the
/// channel's state. **Completion is `done` being set — the worker finished —
/// never the channel emptying or disconnecting:** a detached worker (a
/// `spawn`ed server, a live background `agent`) may hold a sender clone forever
/// and keep the channel non-empty, but it never decides the turn is over,
/// because the loop stops on the explicit `done` flag, not on the channel.
/// This is the daemon-task-hang fix, factored so the two drivers cannot drift
/// on it.
///
/// `done` is checked *before each receive*, so the pass ends the moment the
/// worker finishes even while concurrent background producers keep the channel
/// full — it does not wait for a momentarily-empty channel, which under a
/// background flood would never come. On `done` it drains the buffered batch up
/// to `max` (so the caller can render a final frame including the worker's last
/// events) and returns [`Pass::Stop`]; any further in-flight background events
/// are left for the idle drainer. `None` `max` drains every buffered event
/// (headless, which has nothing to render between them); `Some(n)` caps one
/// pass so a flood cannot starve the TUI's input poll between passes, reporting
/// [`Pass::More`] so the caller drains again. Disconnect also stops.
pub(crate) fn drain_pass(
    rx: &Receiver<Event>,
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

/// One presentation surface.  [`Self::handle`] consumes a single event
/// synchronously; [`Self::drive`] drains the channel until the worker signals
/// completion through `done`.  Completion is an explicit control-flow fact —
/// the worker finished — *not* the channel disconnecting: a detached worker
/// (a `spawn`ed server) may outlive the turn, but it holds bounded deferred
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

    fn drive(&mut self, rx: &Receiver<Event>, done: &AtomicBool) -> io::Result<()> {
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
/// A worker panic is reported through the still-open [`Emitter`] as a
/// final [`Kind::Error`]; the function returns `None` in that case.
///
/// The channel belongs to `bus`, not to `pump`: a session-lived bus keeps it
/// open across turns (the TUI, so a background `agent` streams a live tab),
/// while a per-turn bus closes it when the worker finishes (headless / tests).
/// Either way completion is explicit — the worker sets `done` after `work`
/// returns (or unwinds), and the drain stops on that flag, never on the
/// channel's state.  A detached worker holding a sender clone forever, on
/// either bus, cannot keep the loop — hence the turn — from ending.
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
                let msg = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&'static str>().map(|s| (*s).into()))
                    .unwrap_or_else(|| "non-string payload".into());
                emit.emit(Kind::Error(format!("{WORKER_PANIC_PREFIX}{msg}")));
            }
            // Signal completion before the worker's `emit` (and its sender
            // clone) drops: the turn is over because the worker finished.
            done_ref.store(true, Ordering::Release);
            r.ok()
        });
        sink.drive(bus.rx(), done_ref)?;
        Ok(h.join().ok().flatten())
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AgentMessage, Boundary, Emitter, Event, FleetBus, Inbox, InboxMsg, Kind, ParkMode, Pass,
        Sink, Transcript, Turn, drain_pass, pump,
    };
    use crate::cancel;
    use crate::provider::Tuning;
    use std::sync::Arc;

    /// A scheduled-wakeup message with a fresh pending flag, for the inbox
    /// drain tests.
    fn wakeup(label: &str, trigger: &str, prompt: &str) -> InboxMsg {
        InboxMsg::ScheduledWakeup {
            label: label.into(),
            trigger: trigger.into(),
            prompt: prompt.into(),
            pending: Arc::new(AtomicBool::new(true)),
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
    use std::sync::mpsc::channel;
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
        assert!(matches!(inbox.drain_turn(), Some(Turn::Human(s)) if s == "work"));
        assert!(
            !inbox.waiting_for_input(),
            "draining a turn means work has started; yield resumes only at park"
        );

        let worker_inbox = inbox.clone();
        let token = cancel::Token::new();
        let worker_token = token.clone();
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
            matches!(handle.join().expect("parked worker joins"), Some(Turn::Human(s)) if s == "next"),
            "the wakeup delivered the submitted prompt"
        );
        assert!(
            !inbox.waiting_for_input(),
            "taking the turn leaves the root working until it parks again"
        );
    }

    #[test]
    fn inbox_waiting_for_input_ignores_non_human_parks() {
        let inbox = Inbox::new();
        inbox.push_user("work".into());
        assert!(matches!(inbox.drain_turn(), Some(Turn::Human(_))));
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

        token.cancel();
        assert!(
            handle.join().expect("cancelled worker joins").is_none(),
            "non-human parks terminate on cancellation"
        );
    }

    /// The headless default [`Sink::drive`] and the TUI's `drive_events` share
    /// one completion contract: [`drain_pass`]. It stops when the worker is
    /// *done*, never when the channel empties or disconnects — so a detached
    /// worker holding a sender clone cannot keep a turn alive. Pinning the
    /// shared primitive directly is what keeps the two drivers from drifting on
    /// the daemon-task-hang fix.
    #[test]
    fn drain_pass_stops_on_done_with_a_live_detached_sender() {
        let (tx, rx) = channel::<Event>();
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
        let (tx, rx) = channel::<Event>();
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

    /// The session-lifetime refinement: a finished foreground turn must stop
    /// even while the channel is *non-empty*, because concurrent background
    /// producers (a live async `agent`) keep it full and the old
    /// "stop only on a momentarily-empty channel" rule would never fire. On
    /// `done`, `drain_pass` drains the buffered batch (up to the cap) and
    /// returns `Stop`; the remainder is left for the idle drainer. Without the
    /// fix the foreground turn would hang exactly when a background agent is
    /// flooding the bus.
    #[test]
    fn drain_pass_stops_on_done_even_while_a_background_producer_floods() {
        let (tx, rx) = channel::<Event>();
        let done = AtomicBool::new(false);
        // A background producer keeps sending — the channel is never empty.
        let background = tx.clone();
        for _ in 0..200 {
            background
                .send(Event {
                    id: 9,
                    kind: Kind::Token("x".into()),
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
        // turn, so a detached worker's clone never disconnects the channel.
        let bus = FleetBus::session(Inbox::new());
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

    /// Tool-boundary drain stops at the first slash command.  Slash prompts
    /// wait for the turn boundary, where `/clear`, `/model`, and friends are
    /// interpreted by `handle_slash` instead of shown to the model; the whole
    /// leading user run then coalesces there, preserving the old prompt-queue
    /// join.  The non-slash steering ahead of the command drains mid-turn.
    #[test]
    fn inbox_tool_drain_stops_before_slash_command() {
        let inbox = Inbox::new();
        inbox.push_user("steer first".into());
        inbox.push_user("/clear".into());
        inbox.push_user("after clear".into());

        assert!(
            matches!(inbox.drain_tool().as_slice(), [Turn::Human(s)] if s == "steer first"),
            "the non-slash steering drains; the command stops the run",
        );
        assert!(inbox.drain_tool().is_empty());
        assert!(
            matches!(inbox.drain_turn(), Some(Turn::Human(s)) if s == "/clear\n\nafter clear"),
            "the leading user run coalesces into one human turn",
        );
        assert!(inbox.is_empty());
    }

    /// A scheduled wakeup drains at the tool boundary, marked, alongside the
    /// steering ahead of it — so it reaches the model as soon as the tool
    /// batch settles rather than waiting out the whole turn.
    #[test]
    fn inbox_wakeup_drains_at_tool_boundary_marked() {
        let inbox = Inbox::new();
        inbox.push_user("steer".into());
        inbox.push(wakeup("nightly", "0 3 * * *", "run the tests"));

        assert!(
            matches!(
                inbox.drain_tool().as_slice(),
                [Turn::Human(h), Turn::Wakeup(w)]
                    if h == "steer"
                        && w == "[scheduled 'nightly' · 0 3 * * *] run the tests",
            ),
            "the wakeup drains mid-turn, marked, after the steering",
        );
        assert!(inbox.is_empty());
    }

    /// Asynchronous deliveries — a settled detached agent's `AgentResult`, a
    /// `spawn`'s `Surface`, a `ScheduledWakeup` — drain at the tool boundary
    /// too, in queue order, so a result that settles during a long tool-call
    /// loop reaches the model at the next boundary, not at turn's end.
    #[test]
    fn inbox_tool_drain_takes_async_deliveries() {
        let inbox = Inbox::new();
        // A wakeup that fired, then a barging human, in arrival order.
        inbox.push(wakeup("nightly", "@", "go"));
        inbox.push_user("redirect now".into());
        inbox.push_user("and also this".into());

        assert!(
            matches!(
                inbox.drain_tool().as_slice(),
                [Turn::Wakeup(_), Turn::Human(s)] if s == "redirect now\n\nand also this",
            ),
            "the async wakeup and the coalesced steering both drain, in order",
        );
        assert!(inbox.is_empty());
    }

    #[test]
    fn inbox_agent_message_drains_marked_at_tool_boundary() {
        let inbox = Inbox::new();
        inbox.push(InboxMsg::AgentMessage(AgentMessage {
            from: 7,
            from_title: "review".into(),
            text: "please inspect the parser branch".into(),
        }));

        assert!(matches!(
            inbox.drain_tool().as_slice(),
            [Turn::Message(m)]
                if m.from == 7
                    && m.from_title == "review"
                    && m.text == "please inspect the parser branch"
                    && m.render()
                        == "[EXARCH AGENT 7 MESSAGE: review]\nplease inspect the parser branch\n[/EXARCH]"
        ));
        assert!(inbox.is_empty());
    }

    /// A slash command holds the line: it is the lone turn-boundary message,
    /// so the drain stops at it and everything queued behind — here steering
    /// the human typed after a mid-turn `/model` — stays for the turn boundary,
    /// running after the swap.  Async deliveries ahead of it still drain.
    #[test]
    fn inbox_tool_drain_stops_at_command_barrier() {
        let inbox = Inbox::new();
        inbox.push_user("before".into());
        inbox.push(wakeup("x", "@", "p"));
        inbox.push(InboxMsg::Command("/model".into()));
        inbox.push_user("after model".into());

        // "before" and the wakeup drain; the /model command stops the run, so
        // "after model" stays behind it for the turn boundary.
        assert!(matches!(
            inbox.drain_tool().as_slice(),
            [Turn::Human(b), Turn::Wakeup(_)] if b == "before"
        ));
        assert!(inbox.drain_tool().is_empty());
        assert!(matches!(inbox.drain_turn(), Some(Turn::Command(s)) if s == "/model"));
        assert!(matches!(inbox.drain_turn(), Some(Turn::Human(s)) if s == "after model"));
        assert!(inbox.is_empty());
    }

    /// A queue with no user prompts yields `None`: a sole wakeup is not the
    /// user's draft and stays for the turn boundary.
    #[test]
    fn inbox_pop_back_user_all_no_user_prompts() {
        let inbox = Inbox::new();
        inbox.push(wakeup("x", "@", "p"));
        assert_eq!(inbox.pop_back_user_all(), None, "no user prompts to recall",);
        assert!(matches!(inbox.drain_turn(), Some(Turn::Wakeup(_))));
    }

    /// `pop_back_user_all` extracts every user prompt from the queue — even
    /// ones sandwiched between non-user deliveries — and leaves the non-user
    /// messages in their original order for the turn boundary.
    #[test]
    fn inbox_pop_back_user_all_extracts_all_leaving_non_user_in_order() {
        let inbox = Inbox::new();
        inbox.push_user("first".into());
        inbox.push(wakeup("x", "@", "p"));
        inbox.push_user("second".into());
        inbox.push_user("third".into());
        inbox.push(InboxMsg::Command("/model".into()));
        inbox.push_user("fourth".into());
        assert_eq!(
            inbox.pop_back_user_all(),
            Some(vec![
                "first".to_string(),
                "second".to_string(),
                "third".to_string(),
                "fourth".to_string(),
            ]),
            "all user prompts come back oldest-first, past interspersed deliveries",
        );
        // The non-user messages survive in their original order.
        assert!(matches!(inbox.drain_turn(), Some(Turn::Wakeup(_))));
        assert!(matches!(inbox.drain_turn(), Some(Turn::Command(s)) if s == "/model"));
        assert!(inbox.is_empty());
    }

    /// A detached `spawn` worker's flushed surface batch, terminated by the
    /// `` `done `` event core appends, with a fresh deliver-once latch.
    fn surface(joined: &Arc<Mutex<bool>>) -> InboxMsg {
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
        InboxMsg::Surface {
            id: 0,
            values: vec![done],
            joined: joined.clone(),
        }
    }

    /// Deliver-once at the drain: a `Surface` whose latch is already set (an
    /// `await`/`race` rendered its cards in-turn) is dropped, and the drain
    /// loops to the next deliverable rather than stalling on the suppressed
    /// batch.  An un-joined `Surface` yields a [`Turn::Surface`] and sets the
    /// latch, so a later replay would in turn be suppressed.
    #[test]
    fn inbox_surface_deliver_once_drops_joined_and_surfaces_unjoined() {
        // A `Surface` already joined by an eliminator is dropped; the wakeup
        // queued behind it still surfaces on the same drain.
        let joined = Arc::new(Mutex::new(true));
        let inbox = Inbox::new();
        inbox.push(surface(&joined));
        inbox.push(wakeup("nightly", "@", "go"));
        assert!(
            matches!(inbox.drain_turn(), Some(Turn::Wakeup(_))),
            "a suppressed Surface does not short-circuit the next deliverable"
        );
        assert!(inbox.is_empty());

        // An un-joined `Surface` surfaces and sets its latch.
        let joined = Arc::new(Mutex::new(false));
        let inbox = Inbox::new();
        inbox.push(surface(&joined));
        assert!(
            matches!(inbox.drain_turn(), Some(Turn::Surface { id, .. }) if id == 0),
            "an un-joined Surface yields a Turn::Surface in the root viewport"
        );
        assert!(
            *joined.lock().unwrap(),
            "draining the Surface sets its deliver-once latch"
        );
    }

    /// A `Surface` drains at the tool boundary, like any other delivery, and
    /// `clear` drops a queued batch for free (the deque is emptied), so a
    /// `/clear` between flush and drain delivers nothing.
    #[test]
    fn inbox_surface_drains_at_tool_boundary_and_cleared() {
        let joined = Arc::new(Mutex::new(false));
        let inbox = Inbox::new();
        assert_eq!(surface(&joined).boundary(), Boundary::Tool);

        inbox.push(surface(&joined));
        inbox.clear();
        assert!(
            inbox.drain_tool().is_empty(),
            "a /clear drops the queued batch"
        );

        // A fresh, un-cleared batch surfaces mid-turn.
        let joined = Arc::new(Mutex::new(false));
        inbox.push(surface(&joined));
        assert!(matches!(
            inbox.drain_tool().as_slice(),
            [Turn::Surface { id, .. }] if *id == 0
        ));
    }

    /// The run meter counts a muted child's usage.  Accounting follows the
    /// event, not its emitter: a muted child's display channel is dead (its
    /// receiver dropped, so a sink never sees its `Kind::Usage`), yet it shares
    /// the root's run meter through `muted_child`, so `bus.usage_total()` sums
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

        let bus = FleetBus::per_turn(Inbox::new());
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
        inbox.push(InboxMsg::ScheduledWakeup {
            label: "n".into(),
            trigger: "* * * * *".into(),
            prompt: "go".into(),
            pending: pending.clone(),
        });
        assert!(pending.load(std::sync::atomic::Ordering::Acquire));
        let _ = inbox.drain_turn();
        assert!(
            !pending.load(std::sync::atomic::Ordering::Acquire),
            "draining the wakeup re-opens its schedule"
        );
    }
}
