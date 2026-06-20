//! Agent / frontend boundary.  Workers stamp [`Kind`]s with their
//! [`SessionId`] through an [`Emitter`]; consumers implement [`Sink`].

use crate::card::{Card, IoEvent};
use crate::event::ProviderErrorRecord;
use crate::provider::Usage;
use serde::Serialize;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type SessionId = u64;

/// The identity of an async agent worker.  An async `agent` call forks a
/// child [`Session`](crate::session::Session); its child id *is* its
/// `AgentId`, so the `agents` listing and `agent_cancel` reuse the session
/// identity rather than minting a parallel one.  Opaque: a capability for
/// status and cancellation, not a content hash.
pub type AgentId = SessionId;

/// When a message in the inbox may be drained into the model's context.
///
/// The boundary is a *per-message* property, not a global rule: user
/// steering may barge in at the next tool-call boundary (mid-turn
/// redirection), while a scheduled wakeup or a finished agent is a *fresh*
/// turn and must wait for the turn boundary so it never pollutes the
/// context of a turn already in motion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Boundary {
    /// May drain mid-turn, at a tool-call boundary (user steering).
    Tool,
    /// Drains only at the turn boundary, as its own fresh turn.
    Turn,
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
        match &self.outcome {
            AgentOutcome::Complete => format!("[agent '{}' finished]\n{}", self.title, self.text),
            AgentOutcome::Empty => {
                format!("[agent '{}' finished with no output]", self.title)
            }
            AgentOutcome::Stopped(r) => format!("[agent '{}' stopped: {r}]", self.title),
            AgentOutcome::Cancelled => format!("[agent '{}' was cancelled]", self.title),
            AgentOutcome::Failed(e) => format!("[agent '{}' failed: {e}]", self.title),
        }
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
    /// A scheduled wakeup fired (cron / after).  A fresh, *marked* turn at
    /// the turn boundary — never mid-turn.
    ScheduledWakeup {
        /// The human label the schedule was given (or its id).
        label: String,
        /// The trigger as text — a cron expression or `after <dur>` — for
        /// the marked render and the events record.
        trigger: String,
        /// The natural-language instructions the model acts on.
        prompt: String,
    },
    /// An async agent settled.  A fresh, *marked* turn at the turn boundary.
    AgentResult(AgentResult),
}

impl InboxMsg {
    /// Where this message may be drained.
    pub fn boundary(&self) -> Boundary {
        match self {
            InboxMsg::UserSteering(s) if !s.trim_start().starts_with('/') => Boundary::Tool,
            _ => Boundary::Turn,
        }
    }

    /// The text the model sees when this message is drained into context.
    /// User steering is verbatim; the rest render with their source marker
    /// so the model can tell a wakeup or an agent reply from a human.
    fn render(&self) -> String {
        match self {
            InboxMsg::UserSteering(s) => s.clone(),
            InboxMsg::ScheduledWakeup {
                label,
                trigger,
                prompt,
            } => format!("[scheduled '{label}' · {trigger}] {prompt}"),
            InboxMsg::AgentResult(r) => r.render(),
        }
    }

    /// The single-line label for the pending strip the TUI draws above the
    /// prompt: user prompts show their text, the rest show a glyph + source.
    fn strip_label(&self) -> String {
        match self {
            InboxMsg::UserSteering(s) => s.clone(),
            InboxMsg::ScheduledWakeup { label, .. } => format!("⏰ {label}"),
            InboxMsg::AgentResult(r) => format!("● agent {}", r.title),
        }
    }
}

/// A session's inbox: the typed, multi-producer queue the turn driver pulls
/// its next prompt from.
///
/// The TUI owns one producer side (`Enter` while busy); a cron wakeup and a
/// finishing async agent are other producers, each capturing a clone (the
/// inner `Arc` makes a clone share the same queue).  The turn worker drains
/// tool-boundary messages mid-turn; the driver drains the rest at the turn
/// boundary.  A drained message disappears from the pending strip and
/// cannot be delivered twice.
#[derive(Clone, Default)]
pub struct Inbox {
    inner: Arc<Mutex<VecDeque<InboxMsg>>>,
}

impl Inbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Post any message (cron wakeup, agent result, …).
    pub fn push(&self, msg: InboxMsg) {
        self.inner
            .lock()
            .expect("inbox lock poisoned")
            .push_back(msg);
    }

    /// Post a user-typed steering prompt — the TUI `Enter`-while-busy path.
    pub fn push_user(&self, prompt: String) {
        self.push(InboxMsg::UserSteering(prompt));
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().expect("inbox lock poisoned").is_empty()
    }

    /// One strip label per pending message, oldest first, for the TUI's
    /// pending-prompt strip.
    pub fn snapshot(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("inbox lock poisoned")
            .iter()
            .map(InboxMsg::strip_label)
            .collect()
    }

    /// Pull the newest pending *user* prompt back out for editing — but
    /// only if the tail is user steering.  A pending wakeup or agent result
    /// is not the user's draft and is never pulled into the editor.
    pub fn pop_back_user(&self) -> Option<String> {
        let mut q = self.inner.lock().expect("inbox lock poisoned");
        match q.back() {
            Some(InboxMsg::UserSteering(_)) => match q.pop_back() {
                Some(InboxMsg::UserSteering(s)) => Some(s),
                _ => unreachable!("tail just checked to be user steering"),
            },
            _ => None,
        }
    }

    /// Mid-turn drain at a tool-call boundary: take the leading run of
    /// tool-boundary messages (user steering that is not a slash command),
    /// rendered and joined.  Stops at the first turn-boundary message.
    pub fn drain_tool(&self) -> Option<String> {
        self.drain_run(|msg| msg.boundary() == Boundary::Tool)
    }

    /// Turn-boundary drain: the next deliverable.  A leading run of *user*
    /// steering coalesces into one prompt (preserving the old whole-queue
    /// join, so a lone `/clear` still reaches the command path); a wakeup or
    /// agent result is delivered on its own, rendered with its marker.
    pub fn drain_turn(&self) -> Option<String> {
        let mut q = self.inner.lock().expect("inbox lock poisoned");
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
                Some(text)
            }
            _ => Some(
                q.pop_front()
                    .expect("front checked present")
                    .render(),
            ),
        }
    }

    /// Drop every pending message — `/clear` rebuilds the root, so neither
    /// queued user prompts nor stale non-human deliveries carry across.
    pub fn clear(&self) {
        self.inner.lock().expect("inbox lock poisoned").clear();
    }

    /// Take the leading run of messages matching `keep`, rendered and joined
    /// by a blank line.  `None` when the front does not match.
    fn drain_run(&self, keep: impl Fn(&InboxMsg) -> bool) -> Option<String> {
        let mut q = self.inner.lock().expect("inbox lock poisoned");
        if q.front().is_none_or(|m| !keep(m)) {
            return None;
        }
        let mut text = String::new();
        while q.front().is_some_and(&keep) {
            let msg = q.pop_front().expect("front checked before pop");
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&msg.render());
        }
        Some(text)
    }
}

pub struct Event {
    pub id: SessionId,
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
        /// Short human-readable label for this session, chosen by the
        /// dispatching agent (ASCII alnum / `-` / `_`, 1–24 chars).
        /// Falls back to `sub-{N}` when omitted or invalid.  The TUI
        /// surfaces it in the tab bar; headless ignores it.
        title: String,
    },
    Died,
    Token(String),
    Boundary,
    Usage(Usage),
    Step(u32),
    /// A transient label for the worker's current synchronous phase —
    /// "rendering context", "waiting for model", "typechecking",
    /// "compacting history".  Emitted before a long op so the frontend
    /// can name what the worker is doing during an otherwise silent gap:
    /// the spinner shows the label (a wedge reads "typechecking…", not a
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
    Dim(String),
    ProviderError(ProviderErrorRecord),
    /// Emitted by the `agent` tool when a subagent finishes — *after*
    /// the child's own `Kind::Died` and *before* the spawn rejoins the
    /// parent's tool result.  The event's session id is the parent
    /// (typically root); the TUI lands the breadcrumb in root's
    /// scrollback regardless of nesting depth, since subagent output
    /// otherwise lives only in its own tab and ages out at `LINGER`.
    SubagentDone {
        title: String,
        /// The subagent's final assistant text — empty when the run
        /// failed or was cancelled.
        text: String,
        /// `None` on success; `Some(reason)` on failure / cancel /
        /// stop, where reason is rendered next to the title.
        error: Option<String>,
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
    /// the mark tree erases, so `transcript.jsonl` records the effect itself
    /// beside its presentation.
    ///
    /// [`shell_eval`]: crate::shell_eval
    Io { event: IoEvent, card: Card },
}

/// One located change within a file, carried by a [`crate::card::Mark::Diff`]:
/// the line range beginning at `start` is rewritten from `del` to `add`, with the
/// unchanged `before` and `after` lines the kit captured as surrounding
/// context.  `start` is the 1-indexed line where the change begins; the
/// sink derives every rendered line number from it and the row counts, so
/// removed lines keep their pre-edit numbers and added / context lines
/// take their post-edit ones.
#[derive(Clone, Debug, Serialize)]
pub struct Hunk {
    pub start: u32,
    pub before: Vec<String>,
    pub del: Vec<String>,
    pub add: Vec<String>,
    pub after: Vec<String>,
}

#[derive(Clone)]
pub struct Emitter {
    tx: Sender<Event>,
    id: SessionId,
    inbox: Inbox,
}

impl Emitter {
    pub fn new(tx: Sender<Event>, id: SessionId) -> Self {
        Self::with_inbox(tx, id, Inbox::new())
    }

    pub fn with_inbox(tx: Sender<Event>, id: SessionId, inbox: Inbox) -> Self {
        Self { tx, id, inbox }
    }

    pub fn child(&self, id: SessionId) -> Self {
        Self {
            tx: self.tx.clone(),
            id,
            inbox: self.inbox.clone(),
        }
    }

    pub fn emit(&self, kind: Kind) {
        let _ = self.tx.send(Event { id: self.id, kind });
    }

    /// The session inbox, for a producer the turn starts (a cron schedule,
    /// an async agent) that must post to it after the turn ends — it
    /// captures this clone, which shares the same queue.
    pub fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }

    /// Mid-turn tool-boundary drain of user steering.
    pub fn drain_tool_steering(&self) -> Option<String> {
        self.inbox.drain_tool()
    }
}

/// How often the completion-aware drain loop wakes to re-check the `done`
/// flag while no event is arriving.  Small enough that a turn returns
/// promptly after its worker finishes, large enough not to spin.
const DRAIN_POLL: Duration = Duration::from_millis(10);

/// The verdict of one [`drain_pass`]: the explicit-done completion contract,
/// shared by every driver.
pub(crate) enum Pass {
    /// The worker is done (or the channel disconnected) and every buffered
    /// event has been handled — render a final frame and return.
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
/// never the channel disconnecting:** a detached worker (a `spawn`ed server)
/// may hold a sender clone forever, but it never decides the turn is over,
/// because the loop stops on the explicit `done` flag, not on the last sender
/// dropping. This is the daemon-task-hang fix, factored so the two drivers
/// cannot drift on it.
///
/// `None` `max` drains the channel empty (headless, which has nothing to
/// render between events); `Some(n)` caps one pass so a flood of streamed
/// tokens cannot starve the TUI's input poll between passes. The `done` check
/// fires only once the channel is momentarily empty, so a full batch returns
/// [`Pass::More`] and the caller drains again. Disconnect is a safety net —
/// the common case now that detachment holds no sender — and also stops.
pub(crate) fn drain_pass(
    rx: &Receiver<Event>,
    done: &AtomicBool,
    max: Option<usize>,
    mut handle: impl FnMut(Event),
) -> Pass {
    let mut n = 0usize;
    loop {
        if max.is_some_and(|m| n >= m) {
            return Pass::More;
        }
        match rx.try_recv() {
            Ok(ev) => {
                handle(ev);
                n += 1;
            }
            Err(TryRecvError::Empty) => {
                return if done.load(Ordering::Acquire) {
                    Pass::Stop
                } else {
                    Pass::Idle
                };
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
/// cannot keep the loop alive.  Both drivers route their completion decision
/// through the shared [`drain_pass`]; the only difference is the *frame timer*
/// — the default `drive` blocks on the channel between passes, while the TUI
/// renders and polls keys (see `tui::drive_events`).
pub trait Sink {
    fn handle(&mut self, e: Event);

    fn inbox(&self) -> Inbox {
        Inbox::new()
    }

    fn drive(&mut self, rx: Receiver<Event>, done: &AtomicBool) -> io::Result<()> {
        loop {
            // The shared completion contract. `None` max drains every buffered
            // event — headless has nothing to render between them, so it never
            // needs the TUI's batch cap.
            match drain_pass(&rx, done, None, |ev| self.handle(ev)) {
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

/// Run `work` on a scoped thread, hand the channel to `sink`, join.
/// A worker panic is reported through the still-open [`Emitter`] as a
/// final [`Kind::Error`]; the function returns `None` in that case.
///
/// Completion is explicit: the worker sets `done` after `work` returns (or
/// unwinds), and [`Sink::drive`] stops on that flag rather than on channel
/// disconnect.  A detached worker holding a sender clone forever cannot keep
/// the loop — hence the turn — from ending.
pub fn pump<S, R>(
    sink: &mut S,
    root_id: SessionId,
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
    std::thread::scope(|s| -> io::Result<Option<R>> {
        let (tx, rx) = channel();
        let inbox = sink.inbox();
        let h = s.spawn(move || {
            let emit = Emitter::with_inbox(tx, root_id, inbox);
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&emit)));
            if let Err(p) = &r {
                let msg = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&'static str>().map(|s| (*s).into()))
                    .unwrap_or_else(|| "non-string payload".into());
                emit.emit(Kind::Error(format!("{WORKER_PANIC_PREFIX}{msg}")));
            }
            // Signal completion before the worker's `emit` (and its sender)
            // drops: the turn is over because the worker finished.
            done_ref.store(true, Ordering::Release);
            r.ok()
        });
        sink.drive(rx, done_ref)?;
        Ok(h.join().ok().flatten())
    })
}

#[cfg(test)]
mod tests {
    use super::{Boundary, Emitter, Event, Inbox, InboxMsg, Kind, Pass, Sink, drain_pass, pump};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::channel;
    use std::time::{Duration, Instant};

    /// The headless default [`Sink::drive`] and the TUI's `drive_events` share
    /// one completion contract: [`drain_pass`]. It stops when the worker is
    /// *done*, never when the channel disconnects — so a detached worker
    /// holding a sender clone cannot keep a turn alive. Pinning the shared
    /// primitive directly is what keeps the two drivers from drifting on the
    /// daemon-task-hang fix.
    #[test]
    fn drain_pass_stops_on_done_with_a_live_detached_sender() {
        let (tx, rx) = channel::<Event>();
        let done = AtomicBool::new(false);
        // A detached holder keeps a sender clone alive forever — the channel
        // never disconnects, exactly as a `spawn`ed server would.
        let holder = tx.clone();

        tx.send(Event {
            id: 0,
            kind: Kind::Step(1),
        })
        .unwrap();
        tx.send(Event {
            id: 0,
            kind: Kind::Step(2),
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
        // Outlives `pump`: holds an `Emitter` clone whose `Sender` keeps the
        // channel from ever disconnecting, exactly as a detached worker would.
        let holder: Mutex<Option<Emitter>> = Mutex::new(None);

        let t0 = Instant::now();
        let r = pump(&mut sink, 0, |emit| {
            *holder.lock().unwrap() = Some(emit.clone());
            emit.emit(Kind::Step(1));
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

    /// Tool-boundary steering drains only the non-command prefix.  Slash
    /// prompts stay for the turn boundary, where `/clear`, `/model`, and
    /// friends are interpreted by `handle_slash` instead of shown to the
    /// model.  The whole leading user run then coalesces at the turn
    /// boundary, preserving the old prompt-queue join.
    #[test]
    fn inbox_tool_drain_stops_before_slash_command() {
        let inbox = Inbox::new();
        inbox.push_user("steer first".into());
        inbox.push_user("/clear".into());
        inbox.push_user("after clear".into());

        assert_eq!(inbox.drain_tool().as_deref(), Some("steer first"));
        assert_eq!(inbox.drain_tool(), None);
        assert_eq!(inbox.drain_turn().as_deref(), Some("/clear\n\nafter clear"));
        assert!(inbox.is_empty());
    }

    /// A scheduled wakeup is a turn-boundary message: it never drains at a
    /// tool boundary, and at the turn boundary it renders marked, on its
    /// own, so the model can tell it from a human prompt.
    #[test]
    fn inbox_wakeup_is_turn_boundary_and_marked() {
        let inbox = Inbox::new();
        inbox.push_user("steer".into());
        inbox.push(InboxMsg::ScheduledWakeup {
            label: "nightly".into(),
            trigger: "0 3 * * *".into(),
            prompt: "run the tests".into(),
        });

        // Tool boundary takes the user prefix only, stopping at the wakeup.
        assert_eq!(inbox.drain_tool().as_deref(), Some("steer"));
        assert_eq!(inbox.drain_tool(), None);
        // Turn boundary delivers the wakeup, rendered marked.
        assert_eq!(
            inbox.drain_turn().as_deref(),
            Some("[scheduled 'nightly' · 0 3 * * *] run the tests"),
        );
        assert!(inbox.is_empty());
    }

    /// A non-user tail is never pulled into the editor by `pop_back_user`,
    /// and a wakeup is reported at the turn boundary only.
    #[test]
    fn inbox_pop_back_user_ignores_non_user_tail() {
        let inbox = Inbox::new();
        inbox.push_user("draft".into());
        inbox.push(InboxMsg::ScheduledWakeup {
            label: "x".into(),
            trigger: "@".into(),
            prompt: "p".into(),
        });
        assert_eq!(inbox.pop_back_user(), None, "tail is a wakeup, not a draft");
        assert_eq!(
            InboxMsg::ScheduledWakeup {
                label: "x".into(),
                trigger: "@".into(),
                prompt: "p".into(),
            }
            .boundary(),
            Boundary::Turn,
        );
    }
}
