//! Agent / frontend boundary.  Workers stamp [`Kind`]s with their
//! [`SessionId`] through an [`Emitter`]; consumers implement [`Sink`].

use crate::event::ProviderErrorRecord;
use crate::provider::Usage;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
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

/// The role a `tasks`-kit work item occupies, surfaced by a `task`-tagged
/// sentinel.  A closed set: an unknown tag is rejected at the sentinel
/// boundary (`crate::shell_eval`) rather than carried as free text and
/// degraded to a dim line at render time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskStatus {
    Open,
    Doing,
    Blocked,
    Done,
}

impl TaskStatus {
    /// Parse a sentinel tag; `None` for an unrecognised role, which the
    /// sentinel parser turns into a rejected (un-surfaced) event.
    pub fn parse(tag: &str) -> Option<Self> {
        match tag {
            "open" => Some(Self::Open),
            "doing" => Some(Self::Doing),
            "blocked" => Some(Self::Blocked),
            "done" => Some(Self::Done),
            _ => None,
        }
    }

    /// The wire tag, for the structured event log and `--output-format json`.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Doing => "doing",
            Self::Blocked => "blocked",
            Self::Done => "done",
        }
    }
}

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
    /// A located diff hunk emitted by a ral kit (typically `edit` in
    /// `agent.ral`).  The kit hands a
    /// `` `patch `` variant to the `surface` builtin; [`shell_eval`]
    /// decodes it onto the bus.  Always rendered on the rail; the user
    /// always wants to see what the agent edited.
    ///
    /// [`shell_eval`]: crate::shell_eval
    Patch {
        path: String,
        hunk: Hunk,
    },
    /// A whole-file write surfaced through a `wrote`-tagged sentinel
    /// line on stderr.  `preview` is a small head of the written
    /// body; `lines` is the total line count of the file as written.
    /// Always shown.
    Wrote {
        path: String,
        lines: u32,
        preview: Vec<String>,
    },
    /// A task-status transition from the `tasks` kit (or any kit
    /// modelling work-items) surfaced through a `task`-tagged
    /// sentinel line.  Always shown.
    Task {
        status: TaskStatus,
        desc: String,
    },
    /// A progress meter surfaced through a `meter`-tagged sentinel
    /// line.  `label` is the noun being counted ("tasks", "tests",
    /// "crates").  Always shown.
    Meter {
        done: u32,
        total: u32,
        label: String,
    },
}

/// One located change within a file, carried by [`Kind::Patch`]: the line
/// range beginning at `start` is rewritten from `del` to `add`, with the
/// unchanged `before` and `after` lines the kit captured as surrounding
/// context.  `start` is the 1-indexed line where the change begins; the
/// sink derives every rendered line number from it and the row counts, so
/// removed lines keep their pre-edit numbers and added / context lines
/// take their post-edit ones.
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

/// One presentation surface.  [`Self::handle`] consumes a single event
/// synchronously; [`Self::drive`] drains the channel until the worker signals
/// completion through `done`.  Completion is an explicit control-flow fact —
/// the worker finished — *not* the channel disconnecting: a detached worker
/// (a `spawn`ed server) may outlive the turn, but it holds bounded deferred
/// surface storage in core, never a clone of this channel's sender, so it
/// cannot keep the loop alive.  The default `drive` polls for `done` between
/// events; the TUI overrides it to interleave redraws and key polls.
pub trait Sink {
    fn handle(&mut self, e: Event);

    fn prompt_queue(&self) -> PromptQueue {
        PromptQueue::new()
    }

    fn drive(&mut self, rx: Receiver<Event>, done: &AtomicBool) -> io::Result<()> {
        loop {
            match rx.recv_timeout(DRAIN_POLL) {
                Ok(ev) => self.handle(ev),
                // The worker finished: drain anything it buffered, then stop —
                // regardless of senders still held by detached workers.
                Err(RecvTimeoutError::Timeout) if done.load(Ordering::Acquire) => {
                    while let Ok(ev) = rx.try_recv() {
                        self.handle(ev);
                    }
                    return Ok(());
                }
                Err(RecvTimeoutError::Timeout) => {}
                // Safety net: every sender dropped (the common case now that
                // detachment holds no sender).  Drain and finish.
                Err(RecvTimeoutError::Disconnected) => {
                    while let Ok(ev) = rx.try_recv() {
                        self.handle(ev);
                    }
                    return Ok(());
                }
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
    use super::{Emitter, Event, Kind, PromptQueue, Sink, TaskStatus, pump};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

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

    /// Every known role round-trips through `parse`/`tag`, and an unknown
    /// tag is rejected — the sentinel parser turns that `None` into a
    /// dropped event rather than carrying free text downstream.
    #[test]
    fn task_status_parse_round_trips_and_rejects_unknown() {
        for s in [
            TaskStatus::Open,
            TaskStatus::Doing,
            TaskStatus::Blocked,
            TaskStatus::Done,
        ] {
            assert_eq!(TaskStatus::parse(s.tag()), Some(s));
        }
        assert!(TaskStatus::parse("in-progress").is_none());
        assert!(TaskStatus::parse("").is_none());
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
