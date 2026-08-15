//! The inbox's vocabulary: a [`Post`] is what a producer queues, each with the
//! [`Boundary`] at which it may reach the model; draining reduces it to an
//! [`Item`] for the attend loop to render.  The queue itself is `bus::inbox`.

use crate::fleet::schedule::ScheduleId;
use ral_core::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::AgentId;

/// When a message may be drained into the model's context — per message, not a
/// global rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Boundary {
    /// Mid-exchange, as soon as the current tool batch settles.
    Tool,
    /// Only where the session is `ReadyForUser`: a command, or a slash-prefixed
    /// steering line keeping its place beside one.
    Exchange,
}

/// How an async agent settled, reduced to what the parent's synthetic item
/// needs to say; the provider-message detail stays in the child's own log.
#[derive(Clone, Debug)]
pub enum AgentOutcome {
    /// Finished with a final answer, carried in [`AgentResult::text`].
    Complete,
    Empty,
    /// Stopped for a non-routine reason (content filter, step cap, …).
    Stopped(String),
    /// By `agent-cancel`, `/clear`, or the worker ceiling.
    Cancelled,
    /// A provider error or a panic.
    Failed(String),
}

impl AgentOutcome {
    /// The `(body, error)` a `↘` subagent breadcrumb shows.  Every consumer
    /// of `record::Display::SubagentDone` — transcript, headless stderr, the
    /// TUI — reduces through here, so the three render alike.
    pub(crate) fn breadcrumb(&self, text: &str) -> (String, Option<String>) {
        match self {
            Self::Complete => (text.to_string(), None),
            Self::Empty => (String::new(), None),
            Self::Stopped(r) => (String::new(), Some(r.clone())),
            Self::Cancelled => (String::new(), Some("cancelled".into())),
            Self::Failed(e) => (String::new(), Some(e.clone())),
        }
    }

    /// The marked text the model sees when a settled agent drains.
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

/// The settle record an async agent posts to its parent's inbox, turned to
/// prose only at the model boundary.
#[derive(Clone, Debug)]
pub(crate) struct AgentResult {
    pub name: String,
    pub outcome: AgentOutcome,
    pub text: String,
    pub elapsed: Duration,
    /// The session generation that owned the worker.  One older than the live
    /// session — a worker that settled after a `/clear` — is refused by
    /// `Agent::admits` rather than delivered into a rebuilt context.
    pub generation: u64,
}

impl AgentResult {
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
    pub(super) fn render(&self) -> String {
        format!(
            "[EXARCH AGENT {} MESSAGE: {}]\n{}\n[/EXARCH]",
            self.from, self.from_name, self.text
        )
    }
}

/// One typed message waiting in a session's inbox, the inbound twin of the
/// outbound [`crate::bus::Signal`] stream.  Cancellation is deliberately not a
/// variant: control plane and data plane ride separate rails, so a cancellation
/// is unconstructable here by type.
#[derive(Clone, Debug)]
pub(crate) enum Post {
    /// The user typed a prompt mid-exchange.  A slash-prefixed line waits for
    /// the exchange boundary alongside [`Self::Command`] yet is still delivered
    /// as ordinary prompt text ([`Item::Human`]), never interpreted: only
    /// [`Self::Command`] reaches [`Control`](crate::agent::Control).
    UserSteering(String),
    /// A cron or `after` wakeup fired, delivered as a marked injection.
    ScheduledWakeup {
        /// The firing schedule's id, and the inbox's dedupe key: a newer wakeup
        /// replaces a still-queued one for the same schedule — the inbox's own
        /// guarantee of what `pending` below already gives producer-side.
        id: ScheduleId,
        /// The schedule's own label, always caller-named.
        label: String,
        /// The trigger as text — a cron expression or `after <dur>`.
        trigger: String,
        prompt: String,
        /// Set when this message is posted, cleared when it drains: the next
        /// occurrence reads it and skips, so a tick arriving while the previous
        /// wakeup still waits is dropped rather than stacked.
        pending: Arc<AtomicBool>,
        /// The inbox's clear-epoch as the reaper read it when composing this.
        /// Composing and pushing are two steps a `/clear` can fall between, so
        /// pop-time admission refuses a wakeup whose epoch has fallen behind.
        epoch: u64,
    },
    AgentResult(AgentResult),
    AgentMessage(AgentMessage),
    /// The synthetic continuation the agent posts to *itself* when the nudge
    /// registry turns an attempt back — the same exchange continuing, pushed
    /// through the agent's own `Inbox` and never across agents.
    Nudge {
        exchange: u64,
        text: String,
    },
    /// A session-affecting slash command (`/clear`, `/model`, `/compact`,
    /// `/quit`), raw.  The attend loop owns the session it mutates, so it hands
    /// the line to its [`Control`](crate::agent::Control); view-only commands
    /// (`/help`, `/copy`, …) are served frontend-side and never reach here.
    Command(String),
    /// A deferred `spawn` worker's `surface` batch, delivered at settlement —
    /// the un-awaited path.  Stamped with the *root* session id, since a spawn
    /// worker registers no tab of its own.  Already once-only when posted:
    /// core's completion path wins the worker's deliver-once latch first.
    Surface {
        id: AgentId,
        values: Vec<Value>,
        /// The birth generation the deferred sink (`InboxDeferred`,
        /// `shell_eval.rs`) captured at construction.  A worker settling
        /// mid-`/clear` cannot judge its own staleness, so it always posts and
        /// `Agent::admits` drops a stale batch at the consuming edge.
        generation: u64,
    },
}

impl Post {
    pub(super) fn boundary(&self) -> Boundary {
        match self {
            Self::Command(_) => Boundary::Exchange,
            Self::UserSteering(s) if is_slash(s) => Boundary::Exchange,
            _ => Boundary::Tool,
        }
    }

    /// The side effect of draining.  Only a wakeup has one: clearing `pending`
    /// re-opens its schedule, so the overlap-skip holds exactly until taken.
    pub(super) fn on_drain(&self) {
        if let Self::ScheduledWakeup { pending, .. } = self {
            pending.store(false, Ordering::Release);
        }
    }
}

/// Whether `s` is a slash command line.  The inbox's steering merge reads it
/// too: a slash line never folds into an adjacent run, so its boundary lives on.
pub(super) fn is_slash(s: &str) -> bool {
    s.trim_start().starts_with('/')
}

/// The probe/quota source name — the seven-way split `Inbox::source_depths`
/// and the per-source quota check both key on.
pub(super) fn source_name(msg: &Post) -> &'static str {
    match msg {
        Post::UserSteering(_) => "user",
        Post::ScheduledWakeup { .. } => "schedule",
        Post::AgentResult(_) => "agent",
        Post::AgentMessage(_) => "message",
        Post::Nudge { .. } => "nudge",
        Post::Command(_) => "command",
        Post::Surface { .. } => "surface",
    }
}

/// The notice [`Item::Surface`] wakes the model with.  The cards already reached
/// the rail through `agent::attend::announce`, so this only names the outcome.
fn surface_notice(values: &[Value]) -> String {
    let settled = values
        .iter()
        .rev()
        .find_map(crate::bus::card::value_to_done)
        .as_ref()
        .map_or_else(
            || "background block settled".to_string(),
            crate::bus::card::settled_text,
        );
    format!("{settled}. Await its handle for the value.")
}

/// What a drain yields: the model-facing text *and* its source, so the attend
/// loop can render it in its honest medium — a prompt as the user's turn, a
/// wakeup as marked chrome — while the model receives [`Self::text`] unchanged.
#[derive(Clone, Debug)]
pub(crate) enum Item {
    /// A coalesced run of human prompts, verbatim; a slash-prefixed line never
    /// joins the run, and is always its own `Human` item.
    Human(String),
    /// A scheduled wakeup, rendered as marked chrome rather than a prompt-echo.
    /// Its text is never read as a command, even when it starts with `/`.
    Wakeup(String),
    /// An async agent settled — a dialable `↘` subagent block.
    Agent(AgentResult),
    /// A marked message from a peer agent.
    Message(AgentMessage),
    /// The agent's continuation of its own exchange: no human chrome, and it
    /// opens no new exchange.
    Nudge { exchange: u64, text: String },
    /// A raw slash command for the attend loop's [`Control`](crate::agent::Control).
    Command(String),
    /// A detached `spawn` worker's deferred `surface` batch.
    /// `agent::attend::announce` decodes `values` into the *root* viewport
    /// exactly as a live tool run would; [`Self::text`] wakes the model.
    Surface {
        /// The stamped session id.  `Agent::admits` asserts it matches the
        /// draining session's, so a misrouted batch trips there rather than
        /// rendering silently into the wrong viewport.
        id: AgentId,
        values: Vec<Value>,
        /// Carried through unchanged from [`Post::Surface`].
        generation: u64,
    },
}

impl Item {
    /// The text the model sees when this item drains into context.
    pub(crate) fn text(&self) -> String {
        match self {
            Self::Human(s) | Self::Wakeup(s) | Self::Command(s) => s.clone(),
            Self::Nudge { text, .. } => text.clone(),
            Self::Agent(r) => r.render(),
            Self::Message(m) => m.render(),
            Self::Surface { values, .. } => surface_notice(values),
        }
    }

    /// Whether draining this item opens a new exchange, clearing the nudge
    /// latches and a prior exchange's bare interrupt.  A self-nudge is the same
    /// exchange continuing, so it alone does not.
    pub(crate) fn opens_exchange(&self) -> bool {
        !matches!(self, Self::Nudge { .. })
    }

    pub(crate) fn continues(&self) -> Option<u64> {
        match self {
            Self::Nudge { exchange, .. } => Some(*exchange),
            _ => None,
        }
    }
}
