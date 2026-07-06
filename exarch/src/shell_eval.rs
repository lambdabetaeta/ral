//! In-process ral evaluation against a persistent `Shell`.
//!
//! Runs each tool call as a top-level turn under a pushed capabilities
//! frame with stdout/stderr captured.  The capabilities come from the user's grant
//! policy; there is no source-level `grant { … }` wrapper around the
//! model's body — the boundary is enforced by `eval_top_level` plus the
//! pushed frame, not by surface syntax the model could evade.
//!
//! Each tool call's stdout and stderr are captured into in-memory
//! buffers, replayed to the model in conversation history and written to
//! the session log in full.  Nothing streams live to the user; the rail
//! surfaces tool summaries, patches, writes, and tasks instead.

use crate::agent_registry::AgentRegistry;
use crate::bus::{AgentId, Emitter, InboxMsg, Kind, Mailbox};
use crate::card::{done_card, io_card, value_to_card, value_to_done, value_to_io, value_to_pin};
use crate::transcript::Transcript;
use ral_core::Value as RalValue;
use ral_core::types::DeferredSink;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Idle bound of the lease armed on every detached `spawn` worker: a
/// still-running worker is reaped one hour after the last eliminator
/// named its handle — well past the 30 s foreground wall, renewed by any
/// `poll`/`await`/`race` — so an abandoned worker cannot become an
/// immortal zombie while a babysat one stays alive.  `pub(crate)` so the
/// `/resources` fold reads the same constant its lease rows describe.
pub(crate) const DETACHED_WORKER_CEILING: Duration = Duration::from_secs(60 * 60);

/// Absolute backstop of the same lease, measured from spawn: no amount
/// of ritual polling extends a worker past a day.  Observation renews
/// idleness, never age.
pub(crate) const DETACHED_WORKER_BACKSTOP: Duration = Duration::from_secs(24 * 60 * 60);

/// Admission cap on concurrently *running* workers per agent, enforced by
/// core at the spawn door: the 65th spawn is refused with an error naming
/// `await`/`cancel` while 64 still run.  Durable services count (live work
/// is live work); settled entries lingering under retention never block
/// admission.
pub(crate) const LIVE_WORKER_CAP: usize = 64;

/// Retention bound, in ral calls, on a settled worker's unclaimed result:
/// the entry is swept this many calls after
/// [`Agent::run_shell`](crate::agent::Agent)'s per-call epoch sweep first
/// observes it settled.  256 matches the binding lease's scratch expiry
/// (`decisions/260629_agent-binding-reaping`) — the two ledgers read the
/// same ral-call clock.
pub(crate) const SETTLED_WORKER_RETENTION: u64 = 256;

/// Idle bound, in committed ral calls, on the binding-lease ledger armed on
/// every agent shell: a top-level name unused for this many calls is
/// pruned at the next ready boundary (`decisions/260629_agent-binding-reaping`).
/// Reuses the settled-worker retention figure — one ral-call clock, read by
/// both ledgers for their own idle policy.
pub(crate) const BINDING_IDLE_CALLS: u64 = 256;

/// Soft byte threshold on a session-scope install's shallow-size estimate
/// (`Value::shallow_size`): meeting or exceeding this queues a
/// `LargeBindingNotice` — a residency nudge, never an eviction, recommending
/// a file path over captured bytes
/// (`decisions/260705_leases-and-budgets` §"Shell residency is lexical
/// state plus host leases").
pub(crate) const LARGE_BINDING_BYTES: u64 = 1024 * 1024;

/// The prelude baked into this binary at build time by `build.rs`.
pub static PRELUDE: ral_core::driver::BakedPrelude = ral_core::baked_prelude!();

/// A successful tool run, broken into named pieces so the caller can
/// render twice (full / capped) without parsing the rendered form.
pub struct ToolResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub value: Option<String>,
    pub exit: i32,
}

/// What `run_shell` produces.  `Static` is for parse / type errors —
/// already-formatted ariadne text with no further structure to cap.
pub enum Outcome {
    Ran(ToolResult),
    Static(String),
}

/// The class of a pinned register slot in the session mirror.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinKind {
    Ordinary,
    Commitment,
    /// The `services` ledger card: host-authored, like `Commitment`, but
    /// listing every live durable service rather than one commitment's
    /// verdict.
    Service,
}

/// One mirrored pin: the card the user sees plus the key-derived class the
/// nudge layer reads.  The bus and viewport still carry plain `Kind::Pin`;
/// this struct exists only inside the agent's session mirror.
#[derive(Clone, Debug)]
pub struct PinDigest {
    pub(crate) kind: PinKind,
    pub(crate) card: crate::card::Card,
}

impl PinDigest {
    fn new(key: &str, card: crate::card::Card) -> Self {
        Self {
            kind: if is_commitment_pin(key) {
                PinKind::Commitment
            } else if is_service_pin(key) {
                PinKind::Service
            } else {
                PinKind::Ordinary
            },
            card,
        }
    }
}

/// What a settled `commit`/`verify_commitment` child should do to the
/// protected pin register, decided by the worker thread that drove it while
/// it still holds the child's raw structured reply
/// ([`spawn_async`](crate::tools::agent::spawn_async)).  Carried on
/// [`AgentResult`](crate::bus::AgentResult) and applied only on the parent's
/// own thread, at drain (`Agent::settle_commitment`) — the worker thread
/// never holds `&mut Agent` on the parent.
#[derive(Clone, Debug)]
pub enum CommitmentSettle {
    /// A writer formalized a commitment: open this key with this card.
    /// Refused at drain if the key is somehow already live.
    Open {
        key: String,
        card: crate::card::Card,
    },
    /// A verifier passed: clear this key.
    Clear(String),
}

/// A shared, session-owned register of current pinned-state digests, written
/// by the live surface sink as `` `pin ``/`` `unpin `` flow by and read by the
/// nudge facility to describe what the model has pinned.  The session clones a
/// handle into each turn's turn surface sink; `None` (tests, any path with no
/// nudge layer) disables the mirror.  The session is otherwise pin-blind — pins
/// flow past it to the frontend — so this small mirror is how the boundary
/// nudge can name them.
pub type PinDigests = Arc<Mutex<std::collections::BTreeMap<String, PinDigest>>>;

/// Reserved register keyspace for host-projected commitment state.  A model
/// may observe these pins, but ordinary `surface` calls cannot write or clear
/// them; writer/verifier replies must be projected by host code instead.
pub(crate) const COMMITMENT_PIN_PREFIX: &str = "commitment:";

pub(crate) fn is_commitment_pin(key: &str) -> bool {
    key.starts_with(COMMITMENT_PIN_PREFIX)
}

/// Reserved register key for the host-owned durable-service ledger
/// (`Agent::reconcile_service_pins`): one card listing every live durable
/// service.  A model may observe it, but ordinary `surface` calls cannot
/// write or clear it — only the host, reconciling against the live worker
/// registry, authors it.
pub(crate) const SERVICES_PIN_KEY: &str = "services";

pub(crate) fn is_service_pin(key: &str) -> bool {
    key == SERVICES_PIN_KEY
}

/// Decode one surfaced `Value` into the [`Kind`] it renders as — the single
/// decoder both delivery regimes share.  The live foreground sink
/// (the transport event loop) calls it to emit now; the deferred sink's
/// `deliver` calls the *same* function to mint the identical events at the turn
/// boundary.  Three shapes arrive on the one `surface` channel:
///
///   * a structural I/O event core emits (a read, write, exec, or grep) — a
///     `Map` tagged by its `io` field, decoded into a typed [`IoEvent`] and
///     paired with the [`Card`] composed from it ([`Kind::Io`]);
///   * a render document a ral kit composed (a `` `card `` variant of Bertin
///     marks), decoded into a [`Card`] ([`Kind::Card`]); and
///   * the `` `done `` completion event a detached worker flushes at the end of
///     its deferred batch, composed into its one-line outcome [`Card`].
///
/// A fourth shape rides the same channel as a render *disposition*, tried
/// first: a `` `pin ``/`` `unpin `` wrapper carrying *state* rather than an
/// event ([`value_to_pin`]) — a render document keyed to a register slot,
/// overwritten in place on re-pin ([`Kind::Pin`]/[`Kind::Unpin`]) rather than
/// appended to scrollback.
///
/// They cannot collide — io is a `Map`; a card, a `done`, and a pin are
/// distinct `Variant` labels — and the order (pin, then io, then card, then
/// done) is the tried-first sequence, so the raw effect record always reaches
/// the bus beside its rendering.  A value that is none of these returns `None`
/// and is dropped.
///
/// [`Card`]: crate::card::Card
/// [`IoEvent`]: crate::card::IoEvent
pub fn decode_surface(ev: &RalValue) -> Option<Kind> {
    if let Some((key, body)) = value_to_pin(ev) {
        Some(match body {
            Some(card) => Kind::Pin { key, card },
            None => Kind::Unpin { key },
        })
    } else if let Some(event) = value_to_io(ev) {
        let card = io_card(&event);
        Some(Kind::Io { event, card })
    } else if let Some(card) = value_to_card(ev) {
        Some(Kind::Card(card))
    } else {
        value_to_done(ev).map(|outcome| Kind::Card(done_card(&outcome)))
    }
}

fn reject_protected_pin(kind: &Kind, emit: &Emitter) -> bool {
    let key = match kind {
        Kind::Pin { key, .. } | Kind::Unpin { key }
            if is_commitment_pin(key) || is_service_pin(key) =>
        {
            key
        }
        _ => return false,
    };
    let msg = if is_commitment_pin(key) {
        format!(
            "`{key}` is a protected commitment pin; ordinary `surface` calls cannot write or clear it; call `verify_commitment` to check a live commitment"
        )
    } else {
        format!(
            "`{key}` is a protected service-ledger pin; ordinary `surface` calls cannot write or clear it — it is maintained by the host as services are born and settle"
        )
    };
    emit.emit(Kind::Error(msg));
    true
}

/// The deferred half of `surface`: the session-lived [`DeferredSink`] a
/// detached `spawn` worker flushes its buffered batch to at completion.  It is
/// surface's *deferred destination* — not a new channel — so it carries the
/// same ordinary surface vocabulary the live sink does, posted through the
/// session's own [`Mailbox`] as an [`InboxMsg::Surface`] for the host to render
/// at the next turn boundary (the [`Card`]/`Io` events the live path mints now,
/// minted later) — the spawn worker flushes its deferred surface batch into the
/// agent that ran the spawn.  The id it stamps is the **root** session's, so a
/// spawn worker's cards land in the root viewport — a spawn worker registers no
/// tab of its own.
///
/// The generation guard mirrors the async `agent`'s exactly: it captures the
/// [`AgentRegistry`]'s generation at construction and, in [`Self::deliver`],
/// re-reads the live counter.  A `/clear` between spawn and flush bumps that
/// counter (`AgentRegistry::clear`), so a stale batch is dropped before it can
/// reach a rebuilt context — the session epoch the ADR calls for, reusing the
/// one counter rather than minting a parallel one.  The inbox's `clear` empties
/// the deque, so a batch already queued when `/clear` runs is dropped for free.
///
/// [`Card`]: crate::card::Card
struct InboxDeferred {
    /// The session's own inbox sender; a spawn worker flushes its deferred
    /// surface batch into the agent that ran the spawn.
    mailbox: Mailbox,
    /// The root session id, stamped on every batch so a spawn worker's cards
    /// render in the root viewport.
    root: AgentId,
    registry: AgentRegistry,
    /// The registry generation captured at construction; a batch flushed after
    /// a `/clear` advanced it is dropped.
    generation: u64,
    /// Reports a rejected batch through the existing error vocabulary
    /// (`Kind::Error`, recorded straight to the durable trace) — `deliver`
    /// has no caller to return a `Result` to (it runs on the spawn worker's
    /// own completion, not a synchronous tool call), so this is how the drop
    /// stays visible rather than silent
    /// (`decisions/260705_leases-and-budgets`). A `Transcript`, deliberately
    /// not a whole `Emitter`: this sink outlives the turn (installed once,
    /// flushed whenever the worker settles), and an `Emitter` carries a live
    /// bus sender whose lifetime would then wrongly extend with it — the
    /// exact daemon-task-hang shape `drain_pass`'s own doc warns against. A
    /// `Transcript` is just a durable file handle, safe to hold that long.
    transcript: Transcript,
}

impl DeferredSink for InboxDeferred {
    fn deliver(&self, batch: Vec<ral_core::serial::FOValue>) {
        // A `/clear` since this worker was spawned bumped the registry
        // generation; its batch belongs to a context that no longer exists, so
        // drop it rather than post it into the rebuilt session — the deferred
        // twin of the async agent's stale-result rejection.
        if self.registry.generation() != self.generation {
            return;
        }
        // Decode once, totally, at this door: the deferred batch carries
        // first-order values, the inbox renders `Value`s.
        let values = batch.into_iter().map(RalValue::from).collect();
        if let Err(reject) = self.mailbox.push(InboxMsg::Surface {
            id: self.root,
            values,
        }) {
            self.transcript.record(
                self.root,
                &Kind::Error(format!(
                    "a spawn worker's surfaced batch was dropped: {reject}"
                )),
            );
        }
    }
}

/// Build the [`Arc<dyn DeferredSink>`] a tool turn installs: an
/// [`InboxDeferred`] over `emit`'s session inbox, stamping batches with `root`
/// and guarding them with `registry`'s current generation.  Cloned into the
/// worker's turn state by core, so a nested `spawn` inherits it and flushes at
/// its own completion.
pub fn deferred_sink(
    emit: &Emitter,
    root: AgentId,
    registry: &AgentRegistry,
) -> Arc<dyn DeferredSink> {
    Arc::new(InboxDeferred {
        mailbox: emit.mailbox(),
        root,
        registry: registry.clone(),
        generation: registry.generation(),
        transcript: emit.transcript(),
    })
}

/// Evaluate `cmd` against `shell`, wrapped in `caps`, capturing
/// stdout and stderr into buffers.  Returns the result as named pieces
/// so the caller can render it twice — once full for the terminal,
/// once with per-section caps for the conversation history — without
/// having to parse the rendered form back apart.
/// Evaluate `cmd` against `transport`, wrapped in `caps`, capturing
/// stdout and stderr into buffers. Returns the result as an [`Outcome`].
///
/// Routes through the transport seam: builds a `Source` [`Turn`], dispatches
/// it, drains surface events to the bus, and converts the terminal
/// [`Report`] into the structured result.
pub fn run_shell(
    transport: &dyn ral_core::transport::Transport,
    caps: &ral_core::types::Capabilities,
    cmd: &str,
    timeout_secs: u64,
    emit: &Emitter,
    pins: Option<PinDigests>,
) -> Outcome {
    let name = "<tool>";

    emit.emit(Kind::Phase("evaluating".into()));

    // Trace-only timing.
    #[cfg(debug_assertions)]
    let tool_start = std::time::Instant::now();

    // Build the transport-level Turn.
    use ral_core::transport::{Program, Report, Turn};
    use ral_core::{RequestedTerminalAccess, TurnIo, TurnStdin};

    let turn = Turn {
        program: Program::Source(cmd.to_string()),
        script_name: name.to_string(),
        caps: caps.clone(),
        turn_limit: Some(Duration::from_secs(timeout_secs)),
        deferred_lease: Some(ral_core::types::WorkerLease {
            idle: DETACHED_WORKER_CEILING,
            backstop: DETACHED_WORKER_BACKSTOP,
        }),
        worker_cap: Some(LIVE_WORKER_CAP),
        io: TurnIo::Capture,
        terminal: RequestedTerminalAccess::Denied,
        stdin: TurnStdin::Empty,
    };

    // Dispatch and drain to the Report, rendering surface classes to the bus.
    // The live sink tracks pins; the deferred batch (a deferred worker's
    // flush) renders identically but never touches the pin mirror.
    let report = ral_core::transport::dispatch_to_report(
        transport,
        turn,
        |val| {
            let live_val = RalValue::from(val);
            if let Some(kind) = decode_surface(&live_val) {
                if reject_protected_pin(&kind, emit) {
                    return;
                }
                if let Some(pins) = &pins
                    && let Ok(mut m) = pins.lock()
                {
                    match &kind {
                        Kind::Pin { key, card } => {
                            m.insert(key.clone(), PinDigest::new(key, card.clone()));
                        }
                        Kind::Unpin { key } => {
                            m.remove(key);
                        }
                        _ => {}
                    }
                }
                emit.emit(kind);
            }
        },
        |batch| {
            for val in batch {
                let live_val = RalValue::from(val);
                if let Some(kind) = decode_surface(&live_val) {
                    if reject_protected_pin(&kind, emit) {
                        continue;
                    }
                    emit.emit(kind);
                }
            }
        },
        // No desk until the migration ADR lands: a wire engine's enquiry gets
        // the same honest absence the identity path's missing
        // `TurnRequest.desk` answers with.
        |_req| {
            Err(ral_core::transport::EnquiryError {
                message: "this host answers no enquiries".into(),
                status: 1,
            })
        },
    );

    let report = match report {
        Some(r) => r,
        None => {
            return Outcome::Static("internal error: dispatch completed without a Report".into());
        }
    };

    ral_core::dbg_trace!(
        "shell",
        "eval in {:?} (timed_out={})",
        tool_start.elapsed(),
        false
    );

    match report {
        Report::Static { diagnostics } => {
            use ral_core::transport::Diagnostics;
            match diagnostics {
                Diagnostics::Parse(msg) => Outcome::Static(msg),
                Diagnostics::Types(errs) => Outcome::Static(errs.join("\n")),
                Diagnostics::Host(msg) => Outcome::Static(msg),
            }
        }
        Report::Ran {
            result,
            status: _status,
            single_command: _single_command,
            captured,
            timed_out,
        } => {
            let captured = captured.unwrap_or(ral_core::driver::Captured {
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
            let stdout_bytes = captured.stdout;
            let mut stderr_bytes = captured.stderr;

            let (exit, value) = match &result {
                Ok(sv) => {
                    let v = Some(RalValue::from(sv.clone()));
                    (0, v)
                }
                Err(break_) => match break_ {
                    ral_core::transport::Break::Error(msg) => {
                        let e = if timed_out {
                            let msg = format!(
                                "ral tool: timed out after {timeout_secs}s. If the command is simply slow \
                                 and there is nothing to overlap it with, retry with a higher `timeout_secs`. \
                                 If other work can run alongside it, defer it instead (`let h = defer {{ … }}`) \
                                 and let the turn return: the host notifies you at the next turn boundary when \
                                 it settles and renders its output on the rail, and `await $h` gives you its \
                                 value record — you need not poll."
                            );
                            ral_core::types::Error::new(msg, 124)
                        } else {
                            ral_core::types::Error::new(msg.clone(), 1)
                        };

                        // When using WireTransport, the shell is remote; fall back
                        // to an empty source map for error formatting.
                        // TODO Phase 2: include source map in Report.
                        let sources = transport
                            .as_any()
                            .downcast_ref::<ral_core::transport::IdentityTransport>()
                            .map(|t| t.shell_mut().shell.sources().clone())
                            .unwrap_or_default();
                        let exit = ral_core::diagnostic::report_runtime_error(
                            &mut stderr_bytes,
                            &sources,
                            &e,
                            _single_command,
                        );

                        // Add recovery tip for command exits
                        let is_cmd_exit = matches!(
                            &e.status,
                            ral_core::types::Status::Process(
                                ral_core::process::CommandFailure::ExitCode(_)
                            )
                        );
                        if is_cmd_exit {
                            let mut tip = String::from(
                                "\nrecovery: this non-zero exit raised. If the exit code is the tool own \
                                 signal rather than a failure (grep no-match=1, diff differs=1, test false=1, \
                                 valgrind --error-exitcode=N), its stdout/stderr were captured — read them as \
                                 data with `audit { … }`, which does not raise, or catch with \
                                 `try { … } { |err| … }`. For a yes/no check use `succeeds { … }`.",
                            );
                            if !_single_command {
                                tip.push_str(
                                    " A non-zero exit also aborts the rest of this command and discards earlier \
                                     bindings; wrap risky tools in `audit`/`try`, or split them out.",
                                );
                            }
                            tip.push('\n');
                            stderr_bytes.extend_from_slice(tip.as_bytes());
                        }
                        (exit, None)
                    }
                    ral_core::transport::Break::Exit(code) => ((*code).clamp(0, 255), None),
                    #[cfg(unix)]
                    ral_core::transport::Break::Stopped { .. } => (1, None),
                },
            };

            let value_str = value.as_ref().and_then(ral_value_to_text);

            Outcome::Ran(ToolResult {
                stdout: stdout_bytes,
                stderr: stderr_bytes,
                value: value_str,
                exit,
            })
        }
    }
}

/// Render a JSON value as text at JSON-speaking edges.
///
/// A JSON **string** passes through raw, so a markdown report keeps real
/// newlines rather than the escaped `\n` a serializer would emit; any other
/// shape is **pretty-printed** so its structure stays legible; a JSON **null**
/// renders to nothing (`None`).
///
/// The `reply` tool uses this because its argument arrives as JSON already.
/// A `ral` tool's `VALUE` section uses [`ral_value_to_text`] instead, preserving
/// ral's own value syntax rather than detouring through JSON.
pub(crate) fn json_to_text(json: &serde_json::Value) -> Option<String> {
    match json {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => serde_json::to_string_pretty(other).ok(),
    }
}

/// Render a ral value as the text the `VALUE` section carries.
///
/// Top-level strings and bytes are payloads, so they pass through raw: file
/// windows, markdown reports, and captured byte text keep their exact lines.
/// Structured values print in ral surface syntax instead of detouring through
/// JSON, so variants stay variants and records stay records.
fn ral_value_to_text(value: &RalValue) -> Option<String> {
    match value {
        RalValue::Unit => None,
        RalValue::String(s) => Some(s.clone()),
        RalValue::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        other => Some(ral_value(other, 0)),
    }
}

fn ral_value(value: &RalValue, indent: usize) -> String {
    match value {
        RalValue::Unit => "unit".into(),
        RalValue::Bool(b) => b.to_string(),
        RalValue::Int(n) => n.to_string(),
        RalValue::Float(f) => f.to_string(),
        RalValue::String(s) => quote_ral_string(s),
        RalValue::Bytes(bytes) => quote_ral_string(&String::from_utf8_lossy(bytes)),
        RalValue::List(items) => {
            if items.is_empty() {
                return "[]".into();
            }
            let parts = items
                .iter()
                .map(|item| ral_value(item, indent + 1))
                .collect::<Vec<_>>();
            bracketed(parts, indent, "[", "]")
        }
        RalValue::Map(pairs) => {
            if pairs.is_empty() {
                return "[:]".into();
            }
            let parts = pairs
                .iter()
                .map(|(key, value)| format!("{key}: {}", ral_value(value, indent + 1)))
                .collect::<Vec<_>>();
            bracketed(parts, indent, "[", "]")
        }
        RalValue::Variant { label, payload } => match payload {
            None => format!("`{label}"),
            Some(payload) => format!("`{label} {}", ral_value(payload, indent)),
        },
        RalValue::Lambda { .. } | RalValue::Block { .. } | RalValue::Handle(_) => value.to_string(),
    }
}

fn bracketed(parts: Vec<String>, indent: usize, open: &str, close: &str) -> String {
    let inline = format!("{open}{}{close}", parts.join(", "));
    if inline.len() <= 120 && !inline.contains('\n') {
        return inline;
    }

    let pad = "  ".repeat(indent + 1);
    let end_pad = "  ".repeat(indent);
    format!(
        "{open}\n{pad}{}\n{end_pad}{close}",
        parts.join(&format!(",\n{pad}"))
    )
}

fn quote_ral_string(body: &str) -> String {
    let level = quote_bump_level(body).max(1);
    let hashes = "#".repeat(level);
    format!("{hashes}'{body}'{hashes}")
}

fn quote_bump_level(body: &str) -> usize {
    let bytes = body.as_bytes();
    let mut best = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] == b'#' {
                i += 1;
            }
            best = best.max(i - start + 1);
        } else {
            i += 1;
        }
    }
    best
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    //! Documented-semantics tests for exarch's tool-call evaluator.
    //!
    //! These hold a single [`Shell`] across two `run_shell` calls — the
    //! exact harness shape exarch uses between consecutive tool calls in
    //! a session — and verify the three top-level properties that
    //! motivate routing through `evaluator::eval_top_level`:
    //!
    //!   1. `let` bindings persist across calls.
    //!   2. Effects before a failing line still persist; effects after
    //!      the failing line do not appear (the line never ran).
    //!   3. `cd` persists across calls.
    //!
    //! Equivalent end-to-end coverage of the top-level contract itself
    //! lives in `core/tests/top_level_vs_block.rs`; the exarch-tier
    //! tests below pin that `run_shell`'s wrapping (capabilities frame,
    //! stdout/stderr tees, location bookkeeping) does not perturb the
    //! mobile-install contract.

    use super::*;
    use crate::agent_builtins;
    use crate::bus::{BusReceiver, Emitter, Inbox, channel};
    use ral_core::Shell;
    use ral_core::types::Capabilities;

    /// Render a path without a trailing platform separator.  Some hosts
    /// return `"/tmp/"` from `std::env::temp_dir()`; `Shell::cwd()` never
    /// carries a trailing separator, so trimming here makes the
    /// comparison portable while preserving the `/var` ↔ `/private/var`
    /// firmlink fallback used below.  The `len > 1` guard keeps "/"
    /// itself intact.
    fn display_no_trailing_sep(path: &std::path::Path) -> String {
        let s = path.display().to_string();
        if s.len() > 1 {
            s.trim_end_matches(std::path::MAIN_SEPARATOR).to_string()
        } else {
            s
        }
    }

    /// Build a `Shell` that mirrors `bootstrap::boot_shell` without
    /// signal-handler installation (which is global, racey under
    /// `cargo test`, and not under test here).
    fn fresh_shell() -> Shell {
        let mut shell = ral_core::driver::boot_shell(Default::default(), &PRELUDE);
        agent_builtins::install_on(&mut shell);
        agent_builtins::install_agent_library(&mut shell).expect("embedded agent library");
        crate::bootstrap::seed_no_color(&mut shell);
        shell
    }

    /// Make an `Emitter` whose receiver is held alive locally so sends
    /// don't fail.  Tests never assert on what was emitted — they only
    /// need `run_shell` not to error on the send path.
    fn dummy_emitter() -> (Emitter, BusReceiver) {
        let (tx, rx) = channel();
        (Emitter::new(tx, 0), rx)
    }

    /// Drive one tool call through the real `run_shell` entry point.
    /// `Capabilities::root()` matches exarch's least-restricted default,
    /// which lets every test source compile and run without exercising
    /// the OS sandbox (that path is covered separately in
    /// `core/tests/top_level_vs_block.rs`'s sandbox-parity tests).
    /// Run one tool turn through the **real** production [`run_shell`], so the
    /// test path can never drift from what a live tool call does.  The only
    /// thing the helper owns that production does not is the `&mut Shell`: it
    /// moves the shell into a throwaway [`IdentityTransport`], routes the turn
    /// through `run_shell` (which builds the `Turn`, dispatches, drains the
    /// surface stream into `emit`, and computes the exit code — including the
    /// timeout→124 mapping and the full error rendering), then moves the shell
    /// back out so the caller keeps its session across calls.
    fn run_shell_direct(
        shell: &mut ral_core::Shell,
        caps: &Capabilities,
        cmd: &str,
        timeout_secs: u64,
        emit: &Emitter,
    ) -> Outcome {
        // Move the live shell out behind a cheap throwaway so we can hand it to
        // the transport, which owns its `Shell`.  The placeholder is discarded
        // when we swap the real shell back in below.
        let taken = std::mem::replace(
            shell,
            ral_core::Shell::new(ral_core::io::TerminalState::probe_from_env().1),
        );
        let transport = ral_core::transport::IdentityTransport::new(taken);
        let outcome = run_shell(&transport, caps, cmd, timeout_secs, emit, None);
        // Recover the (now-mutated) shell so `let`/`cd`/binding state persists
        // into the caller's next turn — the across-calls contract these tests pin.
        *shell = transport.into_shell();
        outcome
    }

    /// Run one tool turn and return its [`ToolResult`], delegating to
    /// [`run_shell_direct`] so there is exactly one definition of "run a tool
    /// turn."  Panics on a static (parse/type) failure, preserving the old
    /// helper's behaviour so the out-of-scope `witnesses` tests still panic.
    fn run_once(shell: &mut ral_core::Shell, cmd: &str) -> ToolResult {
        let (emit, _rx) = dummy_emitter();
        match run_shell_direct(shell, &Capabilities::root(), cmd, 30, &emit) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        }
    }

    /// Capability frame with an `fs` projection over the whole tree while
    /// admitting ordinary Unix tools.  This mirrors a restrictive exarch
    /// base: the body evaluates locally, and any external command it spawns
    /// is confined per-command under the OS sandbox.
    #[cfg(unix)]
    fn projecting_caps() -> Capabilities {
        Capabilities {
            fs: Some(ral_core::types::FsPolicy {
                read_prefixes: vec!["/".into()],
                write_prefixes: vec!["/".into()],
                deny_paths: Vec::new(),
            }),
            ..Capabilities::root()
        }
    }

    /// `view-text` is the human read primitive: `view-text PATH 1 2` numbers
    /// the first line and tags it with its witness, each row `<n>\t<hash>\t<text>`.
    #[cfg(unix)]
    #[test]
    fn view_tags_lines_with_hash() {
        let mut shell = fresh_shell();
        let tmp = std::env::temp_dir().join(format!("exarch-view-tag-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let path = tmp.join("alpha.txt");
        std::fs::write(&path, "alpha\n").expect("write view fixture");
        let path_str = display_no_trailing_sep(&path);
        let r = run_once(
            &mut shell,
            &format!(
                "let rows = view-text '{path_str}' 1 2; [line: $rows[0][line], hash: $rows[0][hash], text: $rows[0][text]]"
            ),
        );
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(
            r.exit,
            0,
            "view-text must run; stderr was: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let val = r.value.as_deref().expect("view-text must return value");
        assert!(
            val.contains("line: 1"),
            "line renders as a ral field: {val:?}"
        );
        assert!(
            val.contains("text: #'alpha'#"),
            "text renders as a ral field: {val:?}"
        );
        let hash_start = val.find("hash: #'").expect("hash field") + "hash: #'".len();
        let hash = &val[hash_start..hash_start + 7];
        assert_eq!(
            hash.len(),
            7,
            "hash is an `h` tag plus six hex, got {hash:?}"
        );
        assert!(
            hash.starts_with('h') && hash[1..].bytes().all(|b| b.is_ascii_hexdigit()),
            "hash is `h` followed by six hex chars, got {hash:?}"
        );
    }

    /// Two-call sequence: `let persist_n = 41` then `$[$persist_n + 1]`.
    /// The second call must see the binding from the first — locks in
    /// that exarch's per-call `eval_top_level` install survives across
    /// the tool-call boundary.
    #[test]
    fn tool_call_let_persists_across_calls() {
        let mut shell = fresh_shell();
        let _ = run_once(&mut shell, "let persist_n = 41");
        let second = run_once(&mut shell, "return $[$persist_n + 1]");
        assert_eq!(
            second.exit,
            0,
            "second tool call must succeed; stderr was: {}",
            String::from_utf8_lossy(&second.stderr)
        );
        // Tool results render scalars in the direct ral value printer.
        // An Int returns its ordinary surface form.
        assert_eq!(
            second.value.as_deref(),
            Some("42"),
            "expected the second call's returned value to be 42"
        );
    }

    /// Single tool call: `let pre_x = 1; cat /nonexistent; let post_y = 2`.
    /// The middle command fails, so the line fails; a follow-up call
    /// must see `pre_x` defined and `post_y` undefined.  Locks in the
    /// install-on-Error rule across the tool-call boundary.
    #[test]
    fn tool_call_partial_effects_persist_on_error() {
        let mut shell = fresh_shell();
        let failing = run_once(
            &mut shell,
            "let pre_x = 1\ncat /nonexistent\nlet post_y = 2",
        );
        assert_ne!(
            failing.exit, 0,
            "the failing tool call must surface a non-zero exit"
        );

        // Use a second tool call to check what survived: read both
        // bindings via `try` so the test stays green regardless of
        // which one was defined.  An undefined name elaborates to a
        // type error (caught as Static), which is the wrong shape
        // here; instead we materialise the pre-call env into the
        // second call's bindings via the live shell and inspect
        // `mobile.scope` directly.  Same observation, lower noise.
        assert!(
            shell.scope_lookup("pre_x").is_some(),
            "pre-failure `let` must persist into the next tool call"
        );
        assert!(
            shell.scope_lookup("post_y").is_none(),
            "post-failure `let` never ran, must not be present"
        );
    }

    /// `cd <tmp>` in one tool call must be observable to `cwd` in the
    /// next.  Locks in `logical_cwd` riding the mobile across the
    /// tool-call boundary.
    #[test]
    fn tool_call_cd_persists_across_calls() {
        let mut shell = fresh_shell();
        let tmp = std::env::temp_dir();
        let tmp_disp = display_no_trailing_sep(&tmp);
        let cd = run_once(&mut shell, &format!("cd '{tmp_disp}'"));
        assert_eq!(
            cd.exit,
            0,
            "cd should succeed; stderr was: {}",
            String::from_utf8_lossy(&cd.stderr)
        );
        let pwd = run_once(&mut shell, "cwd");
        assert_eq!(pwd.exit, 0, "cwd in the second call should succeed");
        let canon = display_no_trailing_sep(&tmp.canonicalize().unwrap_or(tmp.clone()));
        let got = pwd
            .value
            .as_deref()
            .expect("cwd must return a String tool value");
        assert!(
            got == tmp_disp || got == canon,
            "expected the second call's cwd to be {tmp_disp:?} or {canon:?}, got {got:?}"
        );
    }

    /// Each line is addressed by the smallest context that makes it unique, so
    /// two lines with the same text but different surroundings get distinct
    /// witnesses — what a bare line hash could not do — and even a line deep in
    /// a run of identical lines grows its window to the run's edge and stays
    /// addressable, so `edit` always picks exactly one line.
    #[test]
    fn edit_window_hash_addresses_repeated_lines() {
        let mut shell = fresh_shell();
        let tmp = std::env::temp_dir().join(format!("exarch-window-edit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);

        // Two `target` lines, different context: distinguishable.
        let repeated = tmp.join("repeated.txt");
        let original = "\
section one:
target
    delete me

section two:
target
    keep me
";
        std::fs::write(&repeated, original).expect("write repeated fixture");
        let repeated_str = display_no_trailing_sep(&repeated);
        // `target` is line 2 = index 1; its witness differs from line 6's.
        let edited = run_once(
            &mut shell,
            &format!(
                "let n = line-count '{repeated_str}'\n\
                 let rows = view-text '{repeated_str}' 1 $[$n + 1]\n\
                 edit '{repeated_str}' [[hash: $rows[1][hash], line: 'FIRST']]"
            ),
        );
        assert_eq!(
            edited.exit,
            0,
            "editing the first `target` by its witness must succeed; stderr was: {}",
            String::from_utf8_lossy(&edited.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(&repeated).expect("read repeated fixture"),
            "\
section one:
FIRST
    delete me

section two:
target
    keep me
",
            "only the first `target` changes; the second is untouched"
        );

        // A line buried in a run of identical lines: the adaptive-context
        // witness grows each line's window to the run's boundary, so even the
        // interior is uniquely addressable — the edit picks exactly one line.
        let run = tmp.join("run.txt");
        let run_original = "head\ndup\ndup\ndup\ndup\ndup\ndup\ndup\ndup\ntail\n";
        std::fs::write(&run, run_original).expect("write run fixture");
        let run_str = display_no_trailing_sep(&run);
        let buried = run_once(
            &mut shell,
            &format!(
                "let n = line-count '{run_str}'\n\
                 let rows = view-text '{run_str}' 1 $[$n + 1]\n\
                 edit '{run_str}' [[hash: $rows[5][hash], line: 'Z']]"
            ),
        );
        let after_run = std::fs::read_to_string(&run).expect("read run fixture after edit");
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(
            buried.exit,
            0,
            "a line deep in an identical run is now uniquely addressable; stderr was: {}",
            String::from_utf8_lossy(&buried.stderr)
        );
        assert_eq!(
            after_run, "head\ndup\ndup\ndup\ndup\nZ\ndup\ndup\ndup\ntail\n",
            "exactly the witnessed line in the run changes; the rest stand"
        );
    }

    /// The batch resolves every hash against one read of the file before
    /// it writes, so a batch is atomic and its edits never interfere —
    /// even adjacent lines, which a per-call edit could not touch together
    /// without one invalidating the next. Pins both halves: a batch with
    /// any stale hash writes nothing, and a clean batch of adjacent edits
    /// (replace, delete, and a one-to-many expansion) applies in one pass.
    #[test]
    fn edit_batch_is_atomic_and_non_interfering() {
        let mut shell = fresh_shell();
        let tmp = std::env::temp_dir().join(format!("exarch-batch-edit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let path = tmp.join("batch.txt");
        let original = "\
keep-top
replace-me
delete-me
expand-me
keep-bottom
";
        std::fs::write(&path, original).expect("write batch fixture");
        let path_str = display_no_trailing_sep(&path);

        // One stale hash poisons the whole batch: nothing is written.
        // `hzzzzzz` is not `h` + six hex, so it can match no witness.
        let poisoned = run_once(
            &mut shell,
            &format!(
                "let n = line-count '{path_str}'\n\
                 let rows = view-text '{path_str}' 1 $[$n + 1]\n\
                 edit '{path_str}' [[hash: $rows[1][hash], line: 'X'], [hash: 'hzzzzzz', line: 'Y']]"
            ),
        );
        assert_ne!(poisoned.exit, 0, "a batch with a stale hash must fail");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read after poisoned batch"),
            original,
            "a failed batch must leave the file untouched"
        );

        // A clean batch over adjacent lines: replace, delete, and expand
        // one line into two — all witnessed from the one read.
        let ok = run_once(
            &mut shell,
            &format!(
                "let n = line-count '{path_str}'\n\
                 let rows = view-text '{path_str}' 1 $[$n + 1]\n\
                 edit '{path_str}' [[hash: $rows[1][hash], line: 'REPLACED'], [hash: $rows[2][hash], line: ''], [hash: $rows[3][hash], line: 'X\nY']]"
            ),
        );
        let after = std::fs::read_to_string(&path).expect("read after clean batch");
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(
            ok.exit,
            0,
            "a clean adjacent batch must succeed; stderr was: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
        assert_eq!(
            after,
            "\
keep-top
REPLACED
X
Y
keep-bottom
"
        );
    }

    /// Regression: a witness hash that happens to read as a number must
    /// still round-trip from `view-text` into `edit`. The agent copies the hash
    /// out of a `view-text` result and types it as a *bare* `edit` argument, so
    /// an all-digit hash like `152347` lexes as an `Int` while the hash
    /// `edit` recomputes is a `String`; the witness check then rejects a
    /// correct hash and the agent loops forever re-issuing the same edit.
    /// The leading-zero case (`012345`) is the sharper one: its integer
    /// reading drops the zero, so no string coercion inside `edit` could
    /// recover the original — only an un-numeric witness format can.
    #[cfg(unix)]
    #[test]
    fn edit_accepts_numeric_witness_hash() {
        // The witness `view-text` shows is the adaptive-context hash, not the
        // bare line digest, so mirror that computation here to search for an
        // all-digit one. For a file written as "{content}\n" the line list is
        // [content, ""]; that's shorter than a full ±MIN_RADIUS (5) window, so
        // line 1 (index 0) clamps to the whole file at the floor radius and its
        // witness is line-hash("5:0:" ++ lh(content) ++ lh("")).
        fn lh(s: &str) -> String {
            format!(
                "h{}",
                &blake3::hash(s.trim_end().as_bytes()).to_hex().as_str()[..6]
            )
        }
        fn view_witness_line1(content: &str) -> String {
            lh(&format!("5:0:{}{}", lh(content), lh("")))
        }
        fn line_with_digit_digest(leading_zero: bool) -> String {
            for n in 0u64..2_000_000 {
                let s = format!("witness fixture line {n}");
                let tag = &view_witness_line1(&s)[1..7];
                if tag.bytes().all(|b| b.is_ascii_digit()) && tag.starts_with('0') == leading_zero {
                    return s;
                }
            }
            panic!("no all-digit six-hex witness in search space (leading_zero={leading_zero})");
        }

        fn assert_round_trips(label: &str, content: &str) {
            let mut shell = fresh_shell();
            let tmp = std::env::temp_dir().join(format!(
                "exarch-numeric-witness-{}-{label}",
                std::process::id()
            ));
            let _ = std::fs::create_dir_all(&tmp);
            let path = tmp.join("fixture.txt");
            std::fs::write(&path, format!("{content}\n")).expect("write witness fixture");
            let path_str = display_no_trailing_sep(&path);

            // Read the witness exactly as the agent would: from `view-text`.
            let vr = run_once(
                &mut shell,
                &format!("let rows = view-text '{path_str}' 1 2; $rows[0][hash]"),
            );
            assert_eq!(
                vr.exit,
                0,
                "view-text must read the fixture; stderr was: {}",
                String::from_utf8_lossy(&vr.stderr)
            );
            let witness = vr
                .value
                .as_deref()
                .expect("view-text must return a hash")
                .trim_matches('"')
                .to_string();

            // Feed the hash straight back as a *bare* token, the way the
            // agent copies it out of the read.
            let er = run_once(
                &mut shell,
                &format!("edit '{path_str}' [[hash: {witness}, line: 'REPLACED']]"),
            );
            let after = std::fs::read_to_string(&path).unwrap_or_default();
            let _ = std::fs::remove_dir_all(&tmp);

            assert_eq!(
                er.exit,
                0,
                "witnessed edit with numeric hash {witness:?} must succeed; stderr was: {}",
                String::from_utf8_lossy(&er.stderr)
            );
            assert_eq!(after.trim_end(), "REPLACED");
        }

        assert_round_trips("plain", &line_with_digit_digest(false));
        assert_round_trips("leading-zero", &line_with_digit_digest(true));
    }

    /// The shared decoder both regimes use round-trips each surface class to
    /// its `Kind` — an io `Map` to `Kind::Io`, a `` `card `` variant to
    /// `Kind::Card`, the `` `done `` completion event to a `Kind::Card`, and a
    /// `` `pin ``/`` `unpin `` disposition to `Kind::Pin`/`Kind::Unpin` — and
    /// drops a junk value to `None`.  The foreground sink emits these now; the
    /// deferred sink's `deliver` mints the identical ones at the turn
    /// boundary.
    #[test]
    fn decode_surface_round_trips_each_class() {
        // An io map → Kind::Io.
        assert!(matches!(
            decode_surface(&RalValue::map(vec![
                ("io".into(), RalValue::String("read".into())),
                ("path".into(), RalValue::String("a.rs".into())),
            ])),
            Some(Kind::Io { .. })
        ));
        // A `card` variant → Kind::Card.
        assert!(matches!(
            decode_surface(&RalValue::Variant {
                label: "card".into(),
                payload: Some(Box::new(RalValue::list(vec![]))),
            }),
            Some(Kind::Card(_))
        ));
        // A `done` event → Kind::Card (the done card).
        assert!(matches!(
            decode_surface(&RalValue::Variant {
                label: "done".into(),
                payload: Some(Box::new(RalValue::map(vec![
                    ("cmd".into(), RalValue::String("<block>".into())),
                    (
                        "outcome".into(),
                        RalValue::Variant {
                            label: "ok".into(),
                            payload: Some(Box::new(RalValue::Unit)),
                        },
                    ),
                ]))),
            }),
            Some(Kind::Card(_))
        ));
        // A `pin` wrapper with a non-empty body → Kind::Pin, decoded by value_to_card.
        assert!(matches!(
            decode_surface(&RalValue::Variant {
                label: "pin".into(),
                payload: Some(Box::new(RalValue::map(vec![
                    ("key".into(), RalValue::String("tasks".into())),
                    (
                        "body".into(),
                        RalValue::Variant {
                            label: "card".into(),
                            payload: Some(Box::new(RalValue::list(vec![RalValue::String(
                                "x".into(),
                            )]))),
                        },
                    ),
                ]))),
            }),
            Some(Kind::Pin { .. })
        ));
        // `unpin`, and a `pin` whose body is absent, both → Kind::Unpin.
        for label in ["unpin", "pin"] {
            assert!(matches!(
                decode_surface(&RalValue::Variant {
                    label: label.into(),
                    payload: Some(Box::new(RalValue::map(vec![(
                        "key".into(),
                        RalValue::String("tasks".into()),
                    )]))),
                }),
                Some(Kind::Unpin { .. })
            ));
        }
        // A `pin` whose body is an *empty* card also drops the slot — a pin
        // with nothing to show is the same as `unpin`.
        assert!(matches!(
            decode_surface(&RalValue::Variant {
                label: "pin".into(),
                payload: Some(Box::new(RalValue::map(vec![
                    ("key".into(), RalValue::String("tasks".into())),
                    (
                        "body".into(),
                        RalValue::Variant {
                            label: "card".into(),
                            payload: Some(Box::new(RalValue::list(vec![]))),
                        },
                    ),
                ]))),
            }),
            Some(Kind::Unpin { .. })
        ));
        // A value that is none of these → None.
        assert!(decode_surface(&RalValue::String("nope".into())).is_none());
    }

    #[test]
    fn model_surface_cannot_write_commitment_pins() {
        let (emit, rx) = dummy_emitter();
        let protected = Kind::Pin {
            key: "commitment:abc".into(),
            card: crate::card::Card(Vec::new()),
        };
        assert!(reject_protected_pin(&protected, &emit));
        let event = rx.try_recv().expect("rejection should emit an error");
        assert!(
            matches!(event.kind, Kind::Error(msg) if msg.contains("protected commitment pin")),
            "expected protected-pin diagnostic"
        );

        let (emit, _rx) = dummy_emitter();
        let ordinary = Kind::Unpin {
            key: "tasks".into(),
        };
        assert!(!reject_protected_pin(&ordinary, &emit));
    }

    /// The `services` key is protected exactly like a commitment pin: a
    /// model's own `surface` call cannot write or clear it — only the
    /// host's reconciliation pass may.
    #[test]
    fn model_surface_cannot_write_service_pins() {
        let (emit, rx) = dummy_emitter();
        let protected = Kind::Pin {
            key: SERVICES_PIN_KEY.to_string(),
            card: crate::card::Card(Vec::new()),
        };
        assert!(reject_protected_pin(&protected, &emit));
        let event = rx.try_recv().expect("rejection should emit an error");
        assert!(
            matches!(event.kind, Kind::Error(msg) if msg.contains("protected service-ledger pin")),
            "expected protected-pin diagnostic"
        );
    }

    /// The `InboxDeferred` posts a deferred worker's batch as an
    /// `InboxMsg::Surface` stamped with the root id — *unless* a `/clear` has
    /// advanced the registry generation since the sink was built, in which
    /// case the batch belongs to a context that no longer exists and is dropped
    /// (the deferred twin of the async agent's stale-result rejection).
    #[test]
    fn inbox_deferred_pushes_then_drops_after_clear() {
        let registry = AgentRegistry::new();
        let inbox = Inbox::new();
        let (tx, _rx) = channel();
        let emit = Emitter::with_mailbox(tx, 7, inbox.mailbox());
        let deferred = deferred_sink(&emit, 7, &registry);

        // A fresh batch reaches the inbox, stamped with the root id (7).
        deferred.deliver(vec![ral_core::serial::FOValue::Unit]);
        match inbox.drain_turn() {
            Some(crate::bus::Turn::Surface { id, .. }) => {
                assert_eq!(id, 7, "the batch is stamped with the root session id")
            }
            other => panic!("a delivered batch surfaces as Turn::Surface, got {other:?}"),
        }

        // A `/clear` bumps the registry generation; the sink captured the
        // old one, so a later flush is dropped rather than posted.
        registry.clear_subtree(7);
        deferred.deliver(vec![ral_core::serial::FOValue::Unit]);
        assert!(
            inbox.is_empty(),
            "a batch flushed after /clear advanced the generation is dropped"
        );
    }

    /// Colour suppression reaches spawned commands through the
    /// environment: a child shell must see `NO_COLOR=1` and
    /// `CLICOLOR_FORCE=0` (see `bootstrap::seed_no_color`).
    #[cfg(unix)]
    #[test]
    fn spawned_commands_inherit_color_suppression() {
        let mut shell = fresh_shell();
        let r = run_once(
            &mut shell,
            "/bin/sh -c 'printf %s \"$NO_COLOR/$CLICOLOR_FORCE\"'",
        );
        assert_eq!(r.exit, 0);
        assert_eq!(String::from_utf8_lossy(&r.stdout), "1/0");
    }

    /// A `timeout` is a hard wall-clock bound, not advisory: a command
    /// that sleeps far longer than its timeout must return in ≈ the
    /// timeout with exit 124, and the *whole* spawned process tree must
    /// be dead afterward — including a grandchild the direct child
    /// forked off.
    ///
    /// The fixture is the `python runtests.py` shape that exposed the
    /// bug: `/bin/sh` forks a `sleep` grandchild that holds the stdout
    /// pipe open, then blocks in `wait`.  Before the fix the standalone
    /// external spawned with `PgidPolicy::Inherit`, so the watchdog's
    /// cancel could only `child.kill()` the `/bin/sh` leader by pid; the
    /// orphaned `sleep` kept the pipe open and the stdout-pump `drain()`
    /// join blocked for the full 30 s. With the external leading its own
    /// process group, the cancel path SIGTERMs (then SIGKILLs) the whole
    /// group, the grandchild dies, the pipe closes, and the call returns
    /// at the 2 s timeout.
    #[cfg(unix)]
    #[test]
    fn timeout_kills_external_subprocess_tree() {
        let mut shell = fresh_shell();
        let (emit, _rx) = dummy_emitter();
        // The grandchild prints its own pid so the test can prove it was
        // reaped, then sleeps far past the timeout; the `/bin/sh` leader
        // blocks in `wait`, holding the call open until the grandchild
        // exits unless the timeout tears the group down.
        let cmd = "/bin/sh -c 'sleep 30 & echo $!; wait'";
        let t0 = std::time::Instant::now();
        let r = match run_shell_direct(&mut shell, &Capabilities::root(), cmd, 2, &emit) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        };
        let elapsed = t0.elapsed();

        // The timeout is enforced: returns at ≈2 s, not the 30 s the
        // `sleep` would have run for, with the conventional exit 124.
        assert!(
            elapsed.as_secs() < 10,
            "timeout must bound wall-clock: returned after {elapsed:?} (sleep was 30s)"
        );
        assert_eq!(
            r.exit, 124,
            "a timed-out call reports the timeout exit code"
        );

        // The forked grandchild must be gone: `kill(pid, 0)` returns
        // ESRCH once it is reaped.  Poll briefly to absorb the tiny
        // window between the group SIGKILL and the kernel reaping it.
        let gc_pid: i32 = String::from_utf8_lossy(&r.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .expect("the grandchild printed its pid on stdout");
        let mut alive = true;
        for _ in 0..50 {
            // SAFETY: signal 0 performs error checking without delivering
            // a signal; it never touches an unrelated process.
            if unsafe { libc::kill(gc_pid as libc::pid_t, 0) } != 0 {
                alive = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !alive,
            "the forked grandchild (pid {gc_pid}) outlived the timeout"
        );
    }

    /// The timeout message names both its budget and the knob to raise.
    /// Mirrors `timeout_kills_external_subprocess_tree`'s fixture — a 2 s
    /// budget over a `sleep 30` — but asserts on the surfaced message rather
    /// than the process tree: exit 124, and a stderr that reports the elapsed
    /// budget ("timed out after 2s") and names `timeout_secs`, the knob the
    /// model raises to give a slow command more room. Those substrings are
    /// load-bearing for steering, so keep them stable.
    #[cfg(unix)]
    #[test]
    fn timeout_message_names_budget_and_knob() {
        let mut shell = fresh_shell();
        let (emit, _rx) = dummy_emitter();
        let cmd = "/bin/sh -c 'sleep 30 & echo $!; wait'";
        let r = match run_shell_direct(&mut shell, &Capabilities::root(), cmd, 2, &emit) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        };
        assert_eq!(
            r.exit, 124,
            "a timed-out call reports the timeout exit code"
        );
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert!(
            stderr.contains("timed out after 2s"),
            "the timeout message names the elapsed budget; stderr was: {stderr}"
        );
        assert!(
            stderr.contains("timeout_secs"),
            "the timeout message names the `timeout_secs` knob; stderr was: {stderr}"
        );
    }

    /// The same timeout must also tear down a sandbox-confined eval child.
    /// That path is blocked in IPC in the parent while the helper runs the
    /// body, so cooperative cancel polling inside the parent is not enough.
    /// The parent must kill the confined helper's subprocess tree out of band.
    #[cfg(unix)]
    #[test]
    fn timeout_kills_sandboxed_subprocess_tree() {
        let mut shell = fresh_shell();
        let (emit, _rx) = dummy_emitter();
        let cmd = "/bin/sh -c 'sleep 30 & echo $!; wait'";
        let t0 = std::time::Instant::now();
        let r = match run_shell_direct(&mut shell, &projecting_caps(), cmd, 2, &emit) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        };
        let elapsed = t0.elapsed();
        let stderr = String::from_utf8_lossy(&r.stderr);
        if r.exit != 124
            && (stderr.contains("sandbox eval")
                || stderr.contains("failed to enter sandbox")
                || stderr.contains("bwrap"))
        {
            eprintln!("skip: OS sandbox unavailable on this host: {stderr}");
            return;
        }

        assert!(
            elapsed.as_secs() < 10,
            "sandboxed timeout must bound wall-clock: returned after {elapsed:?}"
        );
        assert_eq!(
            r.exit, 124,
            "a timed-out sandboxed call reports the timeout exit code; stderr was: {stderr}"
        );

        let gc_pid: i32 = String::from_utf8_lossy(&r.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .expect("the sandboxed grandchild printed its pid on stdout");
        let mut alive = true;
        for _ in 0..50 {
            if unsafe { libc::kill(gc_pid as libc::pid_t, 0) } != 0 {
                alive = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !alive,
            "the sandboxed forked grandchild (pid {gc_pid}) outlived the timeout"
        );
    }

    /// The eval-layer half of the registry cascade: cancelling a forked
    /// session's durable root ([`Shell::cancel_handle`] — the handle
    /// `AgentRegistry` holds per agent) unwinds an in-flight `run_shell`
    /// promptly, long before its `timeout_secs` wall, and tears down the
    /// spawned process tree.  The fixture is the timeout tests' pipe-holding
    /// `sleep` tree under a generous 30 s budget, so only the root cancel
    /// can explain a fast return.  The shell is a `fork_session` child — the
    /// sub-agent shape — which never publishes the process signal slots:
    /// this handle is the only way to stop it, and it must suffice.
    #[cfg(unix)]
    #[test]
    fn root_cancel_unwinds_inflight_run_shell() {
        let mut shell = fresh_shell().fork_session();
        let handle = shell.cancel_handle();
        let (emit, _rx) = dummy_emitter();
        let cmd = "/bin/sh -c 'sleep 30 & echo $!; wait'";
        let t0 = std::time::Instant::now();
        let r = std::thread::scope(|s| {
            let worker = s.spawn(|| {
                match run_shell_direct(&mut shell, &Capabilities::root(), cmd, 30, &emit) {
                    Outcome::Ran(r) => r,
                    Outcome::Static(msg) => panic!("static failure: {msg}"),
                }
            });
            // Let the eval reach the blocking external wait, then cancel the
            // root from outside — the registry cascade's move.
            std::thread::sleep(std::time::Duration::from_millis(300));
            handle.cancel(ral_core::process::CancelCause::Explicit);
            worker.join().expect("run_shell worker")
        });
        let elapsed = t0.elapsed();

        assert!(
            elapsed.as_secs() < 10,
            "a root cancel must unwind the eval promptly: returned after \
             {elapsed:?} (sleep was 30s, budget 30s)"
        );
        // What surfaces depends on where the eval was when the cancel
        // landed: blocked in the child wait, the teardown SIGTERMs the group
        // and the command's signal death is the statement error; between
        // statements, `check` raises the Explicit cause as "cancelled".
        // Either shape is the unwind under test.
        assert_ne!(r.exit, 0, "a cancelled turn must not report success");
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert!(
            stderr.contains("cancelled") || stderr.contains("SIGTERM"),
            "the unwind surfaces the cancel or the torn-down child; stderr was: {stderr}"
        );

        // The compute actually stops: the forked grandchild is reaped, not
        // left grinding as an orphan.
        let gc_pid: i32 = String::from_utf8_lossy(&r.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .expect("the grandchild printed its pid on stdout");
        let mut alive = true;
        for _ in 0..50 {
            // SAFETY: signal 0 performs error checking without delivering
            // a signal; it never touches an unrelated process.
            if unsafe { libc::kill(gc_pid as libc::pid_t, 0) } != 0 {
                alive = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !alive,
            "the forked grandchild (pid {gc_pid}) outlived the root cancel"
        );
    }

    /// A `Bytes` field in a structured result renders to the model as
    /// lossy-UTF-8 text, not the decimal integer array `to-json` uses for
    /// data round-trips.  Captured diagnostics — a job's or `audit` node's
    /// `stderr` — are byte-typed but text by intent; the model must read
    /// the message, not a wall of codes.  Here `to-bytes` mints bytes that
    /// decode to "killed" plus one stray non-UTF-8 byte (0xff): the readable
    /// prefix survives as text and the bad byte becomes a replacement char,
    /// with no decimal code in sight.
    #[test]
    fn byte_fields_render_as_lossy_text_not_decimal() {
        let mut shell = fresh_shell();
        let r = run_once(
            &mut shell,
            "return [stderr: !{to-bytes [107, 105, 108, 108, 101, 100, 255]}]",
        );
        assert_eq!(
            r.exit,
            0,
            "minting a byte value must succeed; stderr was: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let value = r.value.as_deref().expect("structured result");
        assert!(
            value.contains("stderr: #'killed"),
            "the readable prefix renders inside a ral string, got {value:?}"
        );
        assert!(
            !value.contains("107") && !value.contains("255"),
            "no byte renders as a decimal code, got {value:?}"
        );
    }

    /// The programmatic bulk-edit pipeline, end to end and entirely in ral:
    /// `grep-files` finds every `[TODO]` across the tree, the hits fold into a
    /// per-file list, and each file's matching lines are rewritten in one atomic
    /// `edit`.  The hashes come from `view-text`, reading the whole file so
    /// its handles match what `edit` recomputes — no hash is ever read by eye.
    /// A regex `re-replace` turns
    /// each `[TODO]` into `[DONE]` in place.  This is the sweep example in
    /// `data/ral.md`, kept honest by running it.
    #[test]
    fn programmatic_todo_sweep_rewrites_every_match() {
        let mut shell = fresh_shell();
        let tmp = std::env::temp_dir().join(format!("exarch-todo-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp tree");
        std::fs::write(
            tmp.join("a.txt"),
            "alpha [TODO] one\nplain\nbeta [TODO] two\n",
        )
        .expect("write a");
        std::fs::write(tmp.join("b.txt"), "gamma\ndelta [TODO] three\n").expect("write b");
        let tmp_str = display_no_trailing_sep(&tmp);

        let src = format!(
            r#"cd '{tmp_str}'
let hits = grep-files #'\[TODO\]'#
let files = nub !{{map {{ |h| $h[file] }} $hits}}
each {{ |f|
    let lc = line-count $f
    let rows = view-text $f 1 $[$lc + 1]
    let mine = filter {{ |h| equal $h[file] $f }} $hits
    edit $f !{{map {{ |h|
        [ hash: $rows[$[$h[line] - 1]][hash], line: !{{re-replace #'\[TODO\]'# '[DONE]' $h[text]}} ]
    }} $mine}}
}} $files
return !{{length $hits}}"#
        );
        let r = run_once(&mut shell, &src);
        assert_eq!(
            r.exit,
            0,
            "the todo sweep must succeed; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(
            r.value.as_deref(),
            Some("3"),
            "three [TODO] hits across the tree"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.join("a.txt")).expect("read a"),
            "alpha [DONE] one\nplain\nbeta [DONE] two\n",
            "both TODOs in a.txt rewritten in one atomic edit"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.join("b.txt")).expect("read b"),
            "gamma\ndelta [DONE] three\n"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Drive one tool call through `run_shell` with a real bus `Emitter`,
    /// returning the result alongside every `Kind` event captured off the
    /// channel.  The end-to-end coverage harness: it exercises the whole
    /// `core surface → decode_surface → Kind` path the gap tests assert on,
    /// the same wiring `edit_emits_kind_card` and friends use, hoisted so the
    /// coverage tests share it rather than re-threading the channel each time.
    fn run_capturing(shell: &mut Shell, cmd: &str) -> (ToolResult, Vec<crate::bus::Kind>) {
        let (tx, rx) = channel();
        let emit = Emitter::new(tx, 0);
        let result = match run_shell_direct(shell, &Capabilities::root(), cmd, 30, &emit) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static (parse/type) failure: {s}"),
        };
        // Drop the emitter so the channel disconnects and `try_recv` drains
        // cleanly to empty rather than blocking.
        drop(emit);
        let kinds: Vec<crate::bus::Kind> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|ev| ev.kind)
            .collect();
        (result, kinds)
    }

    /// The `IoEvent`s carried by the captured `Kind::Io` events, in order —
    /// the structural effect records the gap tests assert on without caring
    /// about the card composed beside each.
    fn io_events(kinds: &[crate::bus::Kind]) -> Vec<&crate::card::IoEvent> {
        kinds
            .iter()
            .filter_map(|k| match k {
                Kind::Io { event, .. } => Some(event),
                _ => None,
            })
            .collect()
    }

    /// Write `body` into a fresh per-test temp dir and return both the dir and
    /// the display path of the file inside it (no trailing separator), the
    /// fixture shape the redirect/exec coverage tests need.  Mirrors the
    /// scratch-dir pattern the surrounding edit/grep tests already use.
    fn scratch_file(tag: &str, name: &str, body: &str) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("exarch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write scratch fixture");
        let disp = display_no_trailing_sep(&path);
        (dir, disp)
    }

    /// Coverage — the READ door end-to-end: a bare `from-string < a` reads
    /// stdin through one `<` redirect, so exactly one `Kind::Io` carrying a
    /// `Read` event for that path reaches the bus.  No exec card: `from-string`
    /// is a builtin, not an external image.
    #[test]
    fn bare_read_redirect_surfaces_one_read_card() {
        use crate::card::IoEvent;
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("cov-read", "a", "hello\n");

        let (r, kinds) = run_capturing(&mut shell, &format!("from-string < '{path}'"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            r.exit,
            0,
            "the read redirect must succeed; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );

        let ios = io_events(&kinds);
        assert_eq!(
            ios.len(),
            1,
            "a bare `from-string < a` raises exactly one io card, got {ios:?}"
        );
        assert_eq!(
            ios[0],
            &IoEvent::Read { path: path.clone() },
            "the one io event is a read of the redirect path"
        );
    }

    /// Coverage — the WRITE door end-to-end: a bare `to-string "x" > b`
    /// commits an atomic write through one `>` redirect, so exactly one
    /// `Kind::Io` carrying a committed `Write` event reaches the bus.
    #[test]
    fn bare_write_redirect_surfaces_one_committed_write_card() {
        use crate::card::{IoEvent, WriteMode, WriteOutcome};
        let mut shell = fresh_shell();
        // A fresh dir with no fixture file: the write creates the target.
        let dir = std::env::temp_dir().join(format!("exarch-cov-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let path = display_no_trailing_sep(&dir.join("b"));

        let (r, kinds) = run_capturing(&mut shell, &format!("to-string 'x' > '{path}'"));
        let wrote = std::fs::read_to_string(dir.join("b")).ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            r.exit,
            0,
            "the write redirect must succeed; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(wrote.as_deref(), Some("x"), "the write committed to disk");

        let ios = io_events(&kinds);
        assert_eq!(
            ios.len(),
            1,
            "a bare `to-string > b` raises exactly one io card, got {ios:?}"
        );
        assert_eq!(
            ios[0],
            &IoEvent::Write {
                path: path.clone(),
                mode: WriteMode::Write,
                outcome: WriteOutcome::Committed,
                // The committed content rides as `new_bytes`, seeding the write
                // card's content preview.
                new_bytes: Some(b"x".to_vec()),
            },
            "the one io event is a committed write of the redirect path"
        );
    }

    /// Coverage — the EXEC door end-to-end: a bare external command raises
    /// exactly one `Kind::Io` carrying an `Exec` event with the resolved argv
    /// and exit status.  `/usr/bin/true` is the deterministic, always-present
    /// image the core suite already leans on.
    #[cfg(unix)]
    #[test]
    fn bare_external_surfaces_one_exec_card() {
        use crate::card::{ExecOutcome, IoEvent};
        let mut shell = fresh_shell();

        let (r, kinds) = run_capturing(&mut shell, "/usr/bin/true");
        assert_eq!(
            r.exit,
            0,
            "/usr/bin/true must exit zero; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );

        let ios = io_events(&kinds);
        assert_eq!(
            ios.len(),
            1,
            "a bare external raises exactly one io card, got {ios:?}"
        );
        assert_eq!(
            ios[0],
            &IoEvent::Exec {
                argv: vec!["/usr/bin/true".into()],
                outcome: ExecOutcome::Ok,
                status: 0,
            },
            "the one io event is a successful exec of the image"
        );
    }

    /// `view-text` is a host builtin that reads its path in Rust, NOT an
    /// external image: `view-text a 1 2` reads the whole file below the ral line
    /// and surfaces exactly one READ card itself — one logical read surface, no
    /// exec card, like `grep-files` and `edit`.
    #[cfg(unix)]
    #[test]
    fn view_is_a_helper_not_an_exec_image() {
        use crate::card::IoEvent;
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("cov-view", "a", "alpha\nbeta\ngamma\n");

        let (r, kinds) = run_capturing(&mut shell, &format!("view-text '{path}' 1 2"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            r.exit,
            0,
            "view-text must read the fixture; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );

        let ios = io_events(&kinds);
        let reads = ios
            .iter()
            .filter(|e| matches!(e, IoEvent::Read { .. }))
            .count();
        let execs = ios
            .iter()
            .filter(|e| matches!(e, IoEvent::Exec { .. }))
            .count();
        assert_eq!(reads, 1, "view-text surfaces one read card for its file");
        assert_eq!(
            execs, 0,
            "view-text is a host builtin, not an external image — no exec card, got {ios:?}"
        );
    }

    /// Two model operations in one command: `cat < a` installs a `<` redirect
    /// (one READ card) and then runs the external `cat` over that stdin (one
    /// EXEC card) — exactly two io cards, in that order, because the read door
    /// announces eagerly on install, before the body it feeds runs.
    #[cfg(unix)]
    #[test]
    fn cat_redirect_surfaces_read_then_exec_in_order() {
        use crate::card::{ExecOutcome, IoEvent};
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("cov-cat", "a", "one\ntwo\n");

        let (r, kinds) = run_capturing(&mut shell, &format!("/bin/cat < '{path}'"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            r.exit,
            0,
            "cat must read the fixture; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&r.stdout),
            "one\ntwo\n",
            "cat echoes the redirected stdin"
        );

        let ios = io_events(&kinds);
        assert_eq!(
            ios.len(),
            2,
            "cat < a is two logical operations — one read, one exec — got {ios:?}"
        );
        assert_eq!(
            ios[0],
            &IoEvent::Read { path: path.clone() },
            "the read installs first, before the body runs"
        );
        assert_eq!(
            ios[1],
            &IoEvent::Exec {
                argv: vec!["/bin/cat".into()],
                outcome: ExecOutcome::Ok,
                status: 0,
            },
            "then cat execs over that stdin"
        );
    }

    /// Documented boundary — code loading is not turn-time data I/O: sourcing
    /// a small ral file (and `use`-ing one) loads it through `std::fs` below
    /// the redirect frame, so it raises NO io card.  Only the file's own
    /// effects, if any, would surface — here the file is pure bindings, so the
    /// bus stays silent of io events.
    #[test]
    fn sourcing_a_ral_file_raises_no_io_card() {
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("cov-source", "lib.ral", "let answer = 42\n");

        let (sr, source_kinds) = run_capturing(&mut shell, &format!("source '{path}'"));
        assert_eq!(
            sr.exit,
            0,
            "source must load the file; stderr was {:?}",
            String::from_utf8_lossy(&sr.stderr)
        );
        assert!(
            io_events(&source_kinds).is_empty(),
            "code loading is not data I/O — source raises no io card, got {:?}",
            io_events(&source_kinds)
        );

        let (ur, use_kinds) = run_capturing(&mut shell, &format!("use '{path}'"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            ur.exit,
            0,
            "use must load the file; stderr was {:?}",
            String::from_utf8_lossy(&ur.stderr)
        );
        assert!(
            io_events(&use_kinds).is_empty(),
            "use is code loading too — no io card, got {:?}",
            io_events(&use_kinds)
        );
    }

    /// The transcript seam (`transcript::event_record`) is the *operational*
    /// view: a `Kind::Io` projects to `("io", { event })`, keeping the raw
    /// structural effect the rendered card erases — but NOT the card itself,
    /// which is a rendering and belongs to the TUI's `user.log`.  Driven
    /// against `event_record` directly — not a TUI render.
    #[test]
    fn io_event_record_carries_structural_event_not_card() {
        use crate::card::{IoEvent, io_card};
        let event = IoEvent::Write {
            path: "b.rs".into(),
            mode: crate::card::WriteMode::Append,
            outcome: crate::card::WriteOutcome::Committed,
            new_bytes: None,
        };
        let card = io_card(&event);
        let kind = Kind::Io {
            event: event.clone(),
            card,
        };
        let rec = crate::transcript::event_record(7, 3, &kind).expect("an io event records");

        assert_eq!(rec["kind"], "io", "the record is tagged io");
        // The raw structural event survives, tagged by its `io` field with the
        // mode/outcome enums as snake_case strings.
        assert_eq!(rec["event"]["io"], "write");
        assert_eq!(rec["event"]["path"], "b.rs");
        assert_eq!(rec["event"]["mode"], "append");
        assert_eq!(rec["event"]["outcome"], "committed");
        // The rendered card does NOT ride along — it is a presentation, not an
        // operational effect.
        assert!(
            rec.get("card").is_none(),
            "the operational trace drops the rendered card, got {rec:?}"
        );
    }

    /// A JSON string renders raw — no escaping — so a markdown report keeps
    /// real newlines rather than literal `\n`.  This is the shape a `reply`
    /// payload uses.
    #[test]
    fn json_to_text_passes_a_string_through_raw() {
        let v = serde_json::json!("# Report\nline one\nline two");
        assert_eq!(
            super::json_to_text(&v).as_deref(),
            Some("# Report\nline one\nline two"),
        );
    }

    /// A JSON object/array is pretty-printed, so its structure stays legible —
    /// the structured-findings case for `reply`.
    #[test]
    fn json_to_text_pretty_prints_structured_values() {
        let v = serde_json::json!({ "findings": ["a", "b"] });
        let out = super::json_to_text(&v).expect("an object renders");
        assert!(out.contains("\"findings\""));
        assert!(
            out.contains('\n'),
            "pretty-printing keeps the shape on lines"
        );
    }

    /// A JSON null renders to nothing — the empty-reply case that settles
    /// `AgentOutcome::Empty`.
    #[test]
    fn json_to_text_renders_null_to_nothing() {
        assert_eq!(super::json_to_text(&serde_json::Value::Null), None);
    }

    #[test]
    fn ral_value_to_text_passes_top_level_strings_through_raw() {
        let v = RalValue::String("# Report\nline one\nline two".into());
        assert_eq!(
            super::ral_value_to_text(&v).as_deref(),
            Some("# Report\nline one\nline two"),
        );
    }

    #[test]
    fn ral_value_to_text_renders_structures_in_ral_syntax() {
        let v = RalValue::Variant {
            label: "done".into(),
            payload: Some(Box::new(RalValue::map(vec![
                (
                    "files".into(),
                    RalValue::list(vec![RalValue::String("a.rs".into())]),
                ),
                ("tests".into(), RalValue::Int(12)),
            ]))),
        };
        let out = super::ral_value_to_text(&v).expect("variant renders");
        assert!(out.starts_with("`done ["));
        assert!(out.contains("files: [#'a.rs'#]"));
        assert!(out.contains("tests: 12"));
        assert!(!out.contains("\"tag\""));
        assert!(!out.contains("\"payload\""));
    }

    #[test]
    fn ral_value_to_text_uses_multiline_layout_for_wide_structures() {
        let items = (0..64).map(RalValue::Int).collect::<Vec<_>>();
        let out = super::ral_value_to_text(&RalValue::list(items)).expect("list renders");
        assert!(
            out.contains('\n'),
            "wide structures should clip on useful lines"
        );
    }
}
