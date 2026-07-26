//! The inbox's vocabulary: what a producer posts, and what a drain
//! delivers.
//!
//! A [`Post`] is the message as it arrives — user steering, a settled
//! agent, a scheduled wakeup, a slash command — each carrying its own
//! [`Boundary`] at which it may reach the model.  Draining reduces a
//! [`Post`] to an [`Item`], the deliverable the attend loop actually
//! renders.  The queue that holds and drains them lives one module over,
//! in [`crate::bus::inbox`].

use crate::fleet::schedule::ScheduleId;
use ral_core::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::AgentId;

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
pub(super) enum Boundary {
    /// May drain mid-exchange, at a tool-call boundary — everything but a command.
    Tool,
    /// Drains only at the exchange boundary: a slash command, run by `Control`.
    Exchange,
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
    /// the synchronous child's [`crate::bus::Kind::SubagentDone`] and the async result's
    /// fresh item, so the two render as the identical dialable block.
    pub(crate) fn breadcrumb(&self, text: &str) -> (String, Option<String>) {
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
    pub(crate) fn marked_item(&self, name: &str, text: &str) -> String {
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
pub(crate) struct AgentResult {
    pub name: String,
    pub outcome: AgentOutcome,
    pub text: String,
    pub elapsed: Duration,
    /// The session generation that owned the worker.  A result whose
    /// generation is older than the live session (a worker that settled
    /// after a `/clear`) is rejected at drain rather than delivered into a
    /// rebuilt context.
    pub generation: u64,
}

impl AgentResult {
    /// The marked synthetic-item text the model sees when this is drained.
    pub(super) fn render(&self) -> String {
        self.outcome.marked_item(&self.name, &self.text)
    }
}

/// A live agent sent a peer message to another live agent.
#[derive(Clone, Debug)]
pub(crate) struct AgentMessage {
    pub from: AgentId,
    pub from_name: String,
    pub text: String,
}

impl AgentMessage {
    /// The marked synthetic-item text the recipient model sees when this
    /// message drains.
    pub(super) fn render(&self) -> String {
        format!(
            "[EXARCH AGENT {} MESSAGE: {}]\n{}\n[/EXARCH]",
            self.from, self.from_name, self.text
        )
    }
}

/// One typed message waiting in a session's [`crate::bus::Inbox`].
///
/// This is the inbound twin of the outbound [`crate::bus::Kind`] event stream:
/// the inbox holds every producer's message, each carrying its *source* (the
/// variant itself) and its *drain boundary* ([`Post::boundary`]).  A cancellation is
/// deliberately **not** a variant: the control plane (cancel a scope) and
/// the data plane (deliver a message) ride separate rails, so a
/// cancellation is unconstructable here by type.
#[derive(Clone, Debug)]
pub(crate) enum Post {
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
        /// This inbox's clear-epoch ([`crate::bus::Mailbox::epoch`]) as it stood
        /// when the reaper composed this message; the pop-time admission check
        /// refuses a wakeup whose epoch has fallen behind.  The full race is
        /// [`crate::bus::inbox::Shared::epoch`]'s doc.
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
    /// [`crate::bus::Mailbox`], never across agents.
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
    /// Where this message may be drained — see [`Boundary`]'s own doc for the
    /// tool/exchange split.
    pub(super) fn boundary(&self) -> Boundary {
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
    pub(super) fn on_drain(&self) {
        if let Self::ScheduledWakeup { pending, .. } = self {
            pending.store(false, Ordering::Release);
        }
    }
}

/// Whether `s` is a slash command line — trimmed leading whitespace, then a
/// `/`. Shared by [`Post::boundary`] (a slash steering waits for the
/// exchange boundary) and the inbox's steering-merge rule ([`crate::bus::inbox::Shared::try_push`]):
/// a slash line is never folded into an adjacent plain-text run, so its
/// exchange-boundary classification always survives the merge intact.
pub(super) fn is_slash(s: &str) -> bool {
    s.trim_start().starts_with('/')
}

/// The probe/quota source name for one message — the seven-way split
/// [`crate::bus::Inbox::source_depths`] and the quota check ([`crate::bus::inbox::Shared::try_push`]) both
/// key on.
pub(super) fn source_name(msg: &Post) -> &'static str {
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
pub(crate) enum Item {
    /// A coalesced run of human prompts.  A slash-prefixed line is never
    /// part of the run — it is always its own `Human` item, per the
    /// never-merge rule.  Verbatim model text.
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
    /// resulting cards/io into the *root* viewport exactly as a live tool run
    /// would; the model is woken with [`Self::text`]'s notice.
    Surface {
        /// The stamped session id — [`Agent::admits`](crate::agent::Agent::admits)
        /// asserts it matches the draining session's own id, so a batch that
        /// ever reached the wrong inbox trips there rather than rendering
        /// silently into the wrong viewport.
        id: AgentId,
        values: Vec<Value>,
        /// The batch's birth generation, carried through unchanged from
        /// [`Post::Surface`] for [`Agent::admits`] to check.
        generation: u64,
    },
}

impl Item {
    /// The text the model sees when this item is drained into context —
    /// unchanged from what each source always rendered.  A `Surface` is the
    /// host's "your spawn settled" notice — it does not re-narrate the cards
    /// (those rendered on the rail), only names the spawn, its outcome, and
    /// that `await` yields its value.
    pub(crate) fn text(&self) -> String {
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
    pub(crate) fn opens_exchange(&self) -> bool {
        !matches!(self, Self::Nudge(_))
    }
}
