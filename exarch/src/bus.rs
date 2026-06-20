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

/// Prompts submitted while a root turn is already running.
///
/// The TUI owns the producer side (`Enter` while busy); the turn worker owns the
/// consumer side at safe transcript boundaries.  A shared queue keeps the
/// pending strip and the model-visible drain in one place: when the worker drains
/// a prompt, it disappears from the strip and cannot be sent again at the next
/// turn boundary.
#[derive(Clone, Default)]
pub struct PromptQueue {
    inner: Arc<Mutex<VecDeque<String>>>,
}

impl PromptQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, prompt: String) {
        self.inner
            .lock()
            .expect("prompt queue lock poisoned")
            .push_back(prompt);
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .expect("prompt queue lock poisoned")
            .is_empty()
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.inner
            .lock()
            .expect("prompt queue lock poisoned")
            .iter()
            .cloned()
            .collect()
    }
    pub fn pop_back(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("prompt queue lock poisoned")
            .pop_back()
    }

    pub fn drain_joined(&self) -> Option<String> {
        self.drain_prefix(|_| true)
    }

    /// Drain the prefix that may be shown to the model mid-turn.  Slash-prefixed
    /// prompts stay queued for the REPL command path at the outer turn boundary.
    fn drain_steering_joined(&self) -> Option<String> {
        self.drain_prefix(|prompt| !prompt.trim_start().starts_with('/'))
    }

    fn drain_prefix(&self, keep: impl Fn(&str) -> bool) -> Option<String> {
        let mut queue = self.inner.lock().expect("prompt queue lock poisoned");
        if queue.front().is_none_or(|prompt| !keep(prompt)) {
            return None;
        }
        let mut text = String::new();
        while queue.front().is_some_and(|prompt| keep(prompt)) {
            let prompt = queue
                .pop_front()
                .expect("front checked before prompt queue pop");
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&prompt);
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

/// One row of a [`Hunk`]'s unified line list: unchanged context, a removed
/// line, or an inserted line.  Line-level only — no inline word spans.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "tag", content = "text", rename_all = "snake_case")]
pub enum Row {
    Context(String),
    Del(String),
    Add(String),
}

#[derive(Clone)]
pub struct Emitter {
    tx: Sender<Event>,
    id: SessionId,
    prompt_queue: PromptQueue,
}

impl Emitter {
    pub fn new(tx: Sender<Event>, id: SessionId) -> Self {
        Self::with_prompt_queue(tx, id, PromptQueue::new())
    }

    pub fn with_prompt_queue(tx: Sender<Event>, id: SessionId, prompt_queue: PromptQueue) -> Self {
        Self {
            tx,
            id,
            prompt_queue,
        }
    }

    pub fn child(&self, id: SessionId) -> Self {
        Self {
            tx: self.tx.clone(),
            id,
            prompt_queue: self.prompt_queue.clone(),
        }
    }

    pub fn emit(&self, kind: Kind) {
        let _ = self.tx.send(Event { id: self.id, kind });
    }

    pub fn drain_prompt_queue(&self) -> Option<String> {
        self.prompt_queue.drain_steering_joined()
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

    fn prompt_queue(&self) -> PromptQueue {
        PromptQueue::new()
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
        let prompt_queue = sink.prompt_queue();
        let h = s.spawn(move || {
            let emit = Emitter::with_prompt_queue(tx, root_id, prompt_queue);
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
    use super::{Emitter, Event, Kind, Pass, PromptQueue, Sink, drain_pass, pump};
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

    /// Steering drains only the non-command prefix.  Slash-prefixed prompts stay
    /// queued for the REPL boundary, where `/clear`, `/model`, and friends are
    /// interpreted by `handle_slash` instead of being shown to the model.
    #[test]
    fn prompt_queue_steering_drain_stops_before_slash_command() {
        let queue = PromptQueue::new();
        queue.push("steer first".into());
        queue.push("/clear".into());
        queue.push("after clear".into());

        assert_eq!(
            queue.drain_steering_joined().as_deref(),
            Some("steer first")
        );
        assert_eq!(queue.drain_steering_joined(), None);
        assert_eq!(
            queue.drain_joined().as_deref(),
            Some("/clear\n\nafter clear")
        );
    }
}
