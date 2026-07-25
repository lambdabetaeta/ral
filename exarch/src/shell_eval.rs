//! In-process ral evaluation against a persistent `Shell`.
//!
//! Runs each tool call as a top-level run under a pushed capabilities
//! frame with stdout/stderr captured.  The capabilities come from the user's grant
//! policy; there is no source-level `grant { … }` wrapper around the
//! model's body — the boundary is enforced by `eval_top_level` plus the
//! pushed frame, not by surface syntax the model could evade.
//!
//! Each tool call's stdout and stderr are captured into in-memory
//! buffers, replayed to the model in conversation history and written to
//! the session log in full.  Nothing streams live to the user; the rail
//! surfaces tool summaries, patches, writes, and tasks instead.

pub mod builtins;
pub mod skill;
pub mod tools;

use crate::agent::transcript::Transcript;
use crate::bus::card::{
    done_card, io_card, value_to_card, value_to_done, value_to_io, value_to_pin,
};
use crate::bus::{AgentId, Emitter, Kind, Mailbox, Post};
use crate::fleet::registry::AgentRegistry;
use base64::Engine;
use ral_core::Value as RalValue;
use ral_core::serial::FOValue;
use ral_core::types::DeferredSink;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Idle bound of the lease armed on every detached `spawn` worker: a
/// still-running worker is reaped one hour after the last eliminator
/// named its handle — well past the 30 s foreground wall, renewed by any
/// `poll`/`await`/`race` — so an abandoned worker cannot become an
/// immortal zombie while a babysat one stays alive.  `pub(crate)` so the
/// `/resources` fold reads the same constant its lease rows describe.
pub(crate) const DETACHED_WORKER_CEILING: Duration = Duration::from_hours(1);

/// Absolute backstop of the same lease, measured from spawn: no amount
/// of ritual polling extends a worker past a day.  Observation renews
/// idleness, never age.
pub(crate) const DETACHED_WORKER_BACKSTOP: Duration = Duration::from_hours(24);

/// Admission cap on concurrently *running* workers per agent, enforced by
/// core at the spawn door: the 65th spawn is refused with an error naming
/// `await`/`cancel` while 64 still run.  Durable services count (live work
/// is live work); settled entries lingering under retention never block
/// admission.
pub(crate) const LIVE_WORKER_CAP: usize = 64;

/// Retention bound, in ral calls, on a settled worker's unclaimed result:
/// the entry is swept this many calls after
/// [`Agent::run_shell`](crate::agent::Agent)'s per-call epoch sweep first
/// observes it settled.  256 matches the binding lease's scratch expiry —
/// the two ledgers read the same ral-call clock.
pub(crate) const SETTLED_WORKER_RETENTION: u64 = 256;

/// Idle bound, in committed ral calls, on the binding-lease ledger armed on
/// every agent shell: a top-level name unused for this many calls is
/// pruned at the next ready boundary.
/// Reuses the settled-worker retention figure — one ral-call clock, read by
/// both ledgers for their own idle policy.
pub(crate) const BINDING_IDLE_CALLS: u64 = 256;

/// Soft byte threshold on a session-scope install's shallow-size estimate
/// (`Value::shallow_size`): meeting or exceeding this queues a
/// `LargeBindingNotice` — a residency nudge, never an eviction, recommending
/// a file path over captured bytes.
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

/// One mirrored pin: the card the user sees.
///
/// The bus and viewport still carry plain `Kind::Pin`;
/// this struct exists only inside the agent's session mirror.
#[derive(Clone, Debug)]
pub struct PinDigest {
    pub(crate) card: crate::bus::card::Card,
}

impl PinDigest {
    pub(crate) fn new(card: crate::bus::card::Card) -> Self {
        Self { card }
    }
}

/// A shared, session-owned register of current pinned-state digests.
///
/// Written
/// by the live surface sink as `` `pin ``/`` `unpin `` flow by and read by the
/// nudge facility to describe what the model has pinned.  The session clones a
/// handle into each run's surface sink; `None` (tests, any path with no
/// nudge layer) disables the mirror.  The session is otherwise pin-blind — pins
/// flow past it to the frontend — so this small mirror is how the boundary
/// nudge can name them.
pub type PinDigests = Arc<Mutex<std::collections::BTreeMap<String, PinDigest>>>;

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
/// decoder both delivery regimes share.
///
/// The live foreground sink
/// (the transport event loop) calls it to emit now; the deferred sink's
/// `deliver` calls the *same* function to mint the identical events at the exchange
/// boundary.  Four shapes arrive on the one `surface` channel:
///
///   * a structural I/O event core emits (a read, write, exec, or grep) — a
///     `Map` tagged by its `io` field, decoded into a typed [`IoEvent`] and
///     paired with the [`Card`] composed from it ([`Kind::Io`]);
///   * a `` `notice `` core's own ready-boundary housekeeping pushes (a
///     worker reap or an idle-binding prune),
///     decoded into a [`Notice`](crate::bus::card::Notice) and paired with its
///     card ([`Kind::Notice`]);
///   * a render document a ral kit composed (a `` `card `` variant of Bertin
///     marks), decoded into a [`Card`] ([`Kind::Card`]); and
///   * the `` `done `` completion event a detached worker flushes at the end of
///     its deferred batch, decoded into its [`DoneOutcome`](crate::bus::card::DoneOutcome)
///     and paired with its one-line outcome [`Card`] ([`Kind::Done`]).
///
/// A fifth shape rides the same channel as a render *disposition*, tried
/// first: a `` `pin ``/`` `unpin `` wrapper carrying *state* rather than an
/// event ([`value_to_pin`]) — a render document keyed to a register slot,
/// overwritten in place on re-pin ([`Kind::Pin`]/[`Kind::Unpin`]) rather than
/// appended to scrollback.
///
/// They cannot collide — io is a `Map`; a notice, a card, a `done`, and a pin
/// are distinct `Variant` labels — and the order (pin, then io, then notice,
/// then card, then done) is the tried-first sequence, so the raw effect
/// record always reaches the bus beside its rendering.  A value that is none
/// of these returns `None` and is dropped.
///
/// [`Card`]: crate::bus::card::Card
/// [`IoEvent`]: crate::bus::card::IoEvent
pub fn decode_surface(ev: &RalValue) -> Option<Kind> {
    if let Some((key, body)) = value_to_pin(ev) {
        Some(match body {
            Some(card) => Kind::Pin { key, card },
            None => Kind::Unpin { key },
        })
    } else if let Some(event) = value_to_io(ev) {
        let card = io_card(&event);
        Some(Kind::Io { event, card })
    } else if let Some(notice) = crate::bus::card::value_to_notice(ev) {
        let card = crate::bus::card::notice_card(&notice);
        Some(Kind::Notice { notice, card })
    } else if let Some(card) = value_to_card(ev) {
        Some(Kind::Card(card))
    } else {
        value_to_done(ev).map(|outcome| {
            let card = done_card(&outcome);
            Kind::Done { outcome, card }
        })
    }
}

/// Decode one surfaced value and apply the protected-pin guard: the [`Kind`]
/// to emit, or `None` when the value decodes to nothing or is a rejected
/// protected-pin write.  Shared by the live and deferred-batch surface sinks.
pub(crate) fn accepted_surface(val: &RalValue, emit: &Emitter) -> Option<Kind> {
    let kind = decode_surface(val)?;
    (!reject_protected_pin(&kind, emit)).then_some(kind)
}

fn reject_protected_pin(kind: &Kind, emit: &Emitter) -> bool {
    let key = match kind {
        Kind::Pin { key, .. } | Kind::Unpin { key } if is_service_pin(key) => key,
        _ => return false,
    };
    let msg = format!(
        "`{key}` is a protected service-ledger pin; ordinary `surface` calls cannot write or clear it — it is maintained by the host as services are born and settle"
    );
    emit.emit(Kind::Error(msg));
    true
}

/// The deferred half of `surface`: the session-lived [`DeferredSink`] a
/// detached `spawn` worker flushes its buffered batch to at completion.  It is
/// surface's *deferred destination* — not a new channel — so it carries the
/// same ordinary surface vocabulary the live sink does, posted through the
/// session's own [`Mailbox`] as a [`Post::Surface`] for the host to render
/// at the next exchange boundary (the [`Card`]/`Io` events the live path mints now,
/// minted later) — the spawn worker flushes its deferred surface batch into the
/// agent that ran the spawn.  The id it stamps is the **root** session's, so a
/// spawn worker's cards land in the root viewport — a spawn worker registers no
/// tab of its own.
///
/// The generation guard mirrors the async `agent`'s exactly: it captures the
/// [`AgentRegistry`]'s generation at construction and stamps every batch
/// [`Self::deliver`] flushes with it, rather than re-checking the live
/// counter itself — a worker settling mid-`/clear` cannot decide its own
/// staleness (its compose and its push are two separate steps a `/clear` can
/// fall between), so the batch always reaches the inbox and the consuming
/// edge (`Agent::admits`) rejects it there if the generation it carries has
/// fallen behind, exactly as a stale [`AgentResult`](crate::bus::AgentResult)
/// is rejected.
///
/// [`Card`]: crate::bus::card::Card
struct InboxDeferred {
    /// The session's own inbox sender; a spawn worker flushes its deferred
    /// surface batch into the agent that ran the spawn.
    mailbox: Mailbox,
    /// The root session id, stamped on every batch so a spawn worker's cards
    /// render in the root viewport.
    root: AgentId,
    /// The registry generation captured at construction; carried on every
    /// batch for the consuming edge to check.
    generation: u64,
    /// Reports a rejected batch through the existing error vocabulary
    /// (`Kind::Error`, recorded straight to the durable trace) — `deliver`
    /// has no caller to return a `Result` to (it runs on the spawn worker's
    /// own completion, not a synchronous tool call), so this is how the drop
    /// stays visible rather than silent.  A `Transcript`, deliberately
    /// not a whole `Emitter`: this sink outlives the run (installed once,
    /// flushed whenever the worker settles), and an `Emitter` carries a live
    /// bus sender whose lifetime would then wrongly extend with it — the
    /// exact daemon-task-hang shape `drain_pass`'s own doc warns against. A
    /// `Transcript` is just a durable file handle, safe to hold that long.
    transcript: Transcript,
}

impl DeferredSink for InboxDeferred {
    fn deliver(&self, batch: Vec<ral_core::serial::FOValue>) {
        // Decode once, totally, at this door: the deferred batch carries
        // first-order values, the inbox renders `Value`s.
        let values = batch.into_iter().map(RalValue::from).collect();
        if let Err(reject) = self.mailbox.push(Post::Surface {
            id: self.root,
            values,
            generation: self.generation,
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

/// Build the [`Arc<dyn DeferredSink>`] a tool run installs: an
/// [`InboxDeferred`] over `emit`'s session inbox, stamping batches with `root`
/// and `registry`'s generation at this moment.
///
/// Cloned into the
/// worker's run state by core, so a nested `spawn` inherits it and flushes at
/// its own completion.
pub fn deferred_sink(
    emit: &Emitter,
    root: AgentId,
    registry: &AgentRegistry,
) -> Arc<dyn DeferredSink> {
    Arc::new(InboxDeferred {
        mailbox: emit.mailbox(),
        root,
        generation: registry.generation(),
        transcript: emit.transcript(),
    })
}

/// Evaluate `cmd` against `transport`, wrapped in `caps`, capturing
/// stdout and stderr into buffers. Returns the result as an [`Outcome`].
///
/// Routes through the transport seam: builds a `Source` [`Run`], dispatches
/// it, drains surface events to the bus, and converts the terminal
/// [`Report`] into the structured result.
pub(crate) fn run_shell(
    transport: &dyn ral_core::transport::Transport,
    caps: &ral_core::types::Capabilities,
    cmd: &str,
    timeout_secs: u64,
    emit: &Emitter,
    pins: Option<&PinDigests>,
    desk: Option<&Arc<crate::fleet::desk::ExarchDesk>>,
) -> Outcome {
    let name = "<tool>";

    emit.emit(Kind::Phase("evaluating".into()));

    // Trace-only timing.
    #[cfg(debug_assertions)]
    let tool_start = std::time::Instant::now();

    use ral_core::transport::{Program, Report, Run};
    use ral_core::{RequestedTerminalAccess, RunIo, RunStdin};

    let run = Run {
        program: Program::Source(cmd.to_string()),
        script_name: name.to_string(),
        caps: caps.clone(),
        wall: Some(Duration::from_secs(timeout_secs)),
        deferred_lease: Some(ral_core::types::WorkerLease {
            idle: DETACHED_WORKER_CEILING,
            backstop: DETACHED_WORKER_BACKSTOP,
        }),
        worker_cap: Some(LIVE_WORKER_CAP),
        io: RunIo::Capture,
        terminal: RequestedTerminalAccess::Denied,
        stdin: RunStdin::Empty,
    };

    // Dispatch and drain to the Report, rendering surface classes to the bus.
    // The live sink tracks pins; the deferred batch (a deferred worker's
    // flush) renders identically but never touches the pin mirror. Shared
    // with the identity `DeskBinding` adapter (`desk.rs`) so the two paths
    // cannot diverge.
    let applier = crate::fleet::desk::SurfaceApplier {
        emit: emit.clone(),
        pins: pins.cloned(),
    };
    let report = ral_core::transport::dispatch_to_report(
        transport,
        run,
        |val| applier.live(val),
        |batch| applier.deferred(batch),
        // Dead under the identity transport (`transport.set_desk` answers a
        // mid-dispatch `Shell::enquire` directly); live only for a wire
        // engine's `Event::Enquiry`, per `dispatch_to_report`'s own doc.
        // "One handler, two bindings": the identical `ExarchDesk::handle`
        // this closure calls is the one `DeskBinding` wraps for the
        // identity path.
        |req| match desk {
            Some(desk) => desk
                .handle(req)
                .map_err(|e| ral_core::transport::EnquiryError {
                    status: e.exit_code(),
                    message: e.message,
                }),
            None => Err(ral_core::transport::EnquiryError {
                message: "this host answers no enquiries".into(),
                status: 1,
            }),
        },
    );

    let Some(report) = report else {
        return Outcome::Static("internal error: dispatch completed without a Report".into());
    };

    ral_core::dbg_trace!("shell", "eval in {:?}", tool_start.elapsed());

    match report {
        Report::Static { diagnostics } => {
            use ral_core::transport::Diagnostics;
            match diagnostics {
                Diagnostics::Types(errs) => Outcome::Static(errs.join("\n")),
                Diagnostics::Parse(msg) | Diagnostics::Host(msg) => Outcome::Static(msg),
            }
        }
        Report::Ran {
            result,
            status,
            single_command,
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
                    ral_core::transport::Break::Error {
                        rendered,
                        command_exit,
                    } => {
                        if timed_out {
                            // The wall fired: the model needs the remedy, not
                            // the cancellation diagnostic — replace the
                            // rendering wholesale, exit 124 by exarch's own
                            // timeout convention.
                            let msg = format!(
                                "error: ral tool: timed out after {timeout_secs}s. If the command is simply slow \
                                 and there is nothing to overlap it with, retry with a higher `timeout_secs`. \
                                 If other work can run alongside it, defer it instead (`let h = defer {{ … }}`) \
                                 and let the run return: the host notifies you at the next exchange boundary when \
                                 it settles and renders its output on the rail, and `await $h` gives you its \
                                 value record — you need not poll.\n"
                            );
                            stderr_bytes.extend_from_slice(msg.as_bytes());
                            (124, None)
                        } else {
                            // The seam already rendered the full diagnostic —
                            // prefix, exit status, hint, caret — against the
                            // engine's own source map; print it verbatim,
                            // exactly as the REPL does, and take the engine's
                            // once-computed status as the tool exit.
                            stderr_bytes.extend_from_slice(rendered.as_bytes());
                            if *command_exit {
                                let mut tip = String::from(
                                    "\nrecovery: this non-zero exit raised. If the exit code is the tool own \
                                     signal rather than a failure (grep no-match=1, diff differs=1, test false=1, \
                                     valgrind --error-exitcode=N), its stdout/stderr were captured — read them as \
                                     data with `audit { … }`, which does not raise, or catch with \
                                     `try { … } { |err| … }`. For a yes/no check use `succeeds { … }`.",
                                );
                                if !single_command {
                                    tip.push_str(
                                        " A non-zero exit also aborts the rest of this command and discards earlier \
                                         bindings; wrap risky tools in `audit`/`try`, or split them out.",
                                    );
                                }
                                tip.push('\n');
                                stderr_bytes.extend_from_slice(tip.as_bytes());
                            }
                            (status, None)
                        }
                    }
                    ral_core::transport::Break::Exit(code) => (*code, None),
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

/// Render a ral value as the text the `VALUE` section carries.
///
/// Top-level strings and bytes are payloads, so they pass through raw: file
/// windows, markdown reports, and captured byte text keep their exact lines.
/// Structured values print in ral surface syntax instead of detouring through
/// JSON, so variants stay variants and records stay records.
const VALUE_PRINT_PARAMS: ral_core::builtins::PrintParams = ral_core::builtins::PrintParams {
    max_width: 120,
    max_string: 72,
    max_depth: 2,
    min_quote_hashes: 1,
    quote_bytes: true,
};

pub(crate) fn ral_value_to_text(value: &RalValue) -> Option<String> {
    match value {
        RalValue::Unit => None,
        RalValue::String(s) => Some(s.clone()),
        RalValue::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        other => Some(ral_core::builtins::pretty_print(
            other,
            0,
            &VALUE_PRINT_PARAMS,
        )),
    }
}

/// Project a `reply`'s first-order payload to the JSON a user-facing edge
/// (the headless `result`) reads — **not** [`FOValue`]'s own
/// `serde`/[`Serialize`] impl,
/// which is the transport encoding: internally tagged by `kind`, floats
/// carried by IEEE-754 bits (`core/src/serial.rs:31–84`).  That encoding
/// would break the promise this projection keeps: a string reply stays a
/// bare JSON string, and a structured one stays ordinary JSON, not a
/// `{"kind":"string","value":"…"}` wrapper.
///
/// The policy, one arm per [`FOValue`] variant:
/// - `Unit` → `null`; `Bool`/`Int` → the JSON scalar.
/// - `Float` → a JSON number for a finite value; JSON has no representation
///   for NaN or ±∞, so a non-finite float crosses as the string `"NaN"`,
///   `"Infinity"`, or `"-Infinity"`.
/// - `String` → the string, raw.
/// - `Bytes` → a base64-encoded string (JSON has no byte-string type).
/// - `List` → an array; `Map` → an object, key order preserved.
/// - `Variant` with a payload → a single-key object `{label: payload}`;
///   a payload-less variant → the bare label as a string.
///
/// [`Serialize`]: serde::Serialize
pub(crate) fn user_json(v: &FOValue) -> serde_json::Value {
    match v {
        FOValue::Unit => serde_json::Value::Null,
        FOValue::Bool { value } => serde_json::Value::Bool(*value),
        FOValue::Int { value } => serde_json::Value::Number((*value).into()),
        FOValue::Float { value } if value.is_nan() => serde_json::Value::String("NaN".into()),
        FOValue::Float { value } if value.is_infinite() => serde_json::Value::String(
            if *value > 0.0 {
                "Infinity"
            } else {
                "-Infinity"
            }
            .into(),
        ),
        FOValue::Float { value } => serde_json::Number::from_f64(*value)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        FOValue::String { value } => serde_json::Value::String(value.clone()),
        FOValue::Bytes { value } => {
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(value))
        }
        FOValue::List { items } => serde_json::Value::Array(items.iter().map(user_json).collect()),
        FOValue::Map { entries } => serde_json::Value::Object(
            entries
                .iter()
                .map(|(k, v)| (k.clone(), user_json(v)))
                .collect(),
        ),
        FOValue::Variant {
            label,
            payload: Some(p),
        } => {
            let mut m = serde_json::Map::new();
            m.insert(label.clone(), user_json(p));
            serde_json::Value::Object(m)
        }
        FOValue::Variant {
            label,
            payload: None,
        } => serde_json::Value::String(label.clone()),
        #[allow(
            clippy::uninhabited_references,
            reason = "NoExt is uninhabited, so this arm never actually runs; the dereference \
                      exhaustiveness needs is never performed at runtime"
        )]
        FOValue::Ext(x) => match *x {},
    }
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
    use crate::bus::{Emitter, Inbox, channel};
    use crate::shell_eval::builtins;
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
        let mut shell = ral_core::driver::boot_shell(
            ral_core::io::TerminalState::default(),
            &PRELUDE,
            &builtins::host_surface(),
        );
        builtins::install_agent_library(&mut shell).expect("embedded agent library");
        crate::bootstrap::seed_no_color(&mut shell);
        shell
    }

    /// Run one tool run through the **real** production [`run_shell`], so the
    /// test path can never drift from what a live tool call does.  The only
    /// thing the helper owns that production does not is the `&mut Shell`: it
    /// moves the shell into a throwaway [`IdentityTransport`], routes the run
    /// through `run_shell` (which builds the `Run`, dispatches, drains the
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
        let outcome = run_shell(&transport, caps, cmd, timeout_secs, emit, None, None);
        // Recover the (now-mutated) shell so `let`/`cd`/binding state persists
        // into the caller's next run — the across-calls contract these tests pin.
        *shell = transport.into_shell();
        outcome
    }

    /// Run one tool run and return its [`ToolResult`], delegating to
    /// [`run_shell_direct`] under `Capabilities::root()` — exarch's
    /// least-restricted default, letting every test source here compile and
    /// run without exercising the OS sandbox (covered separately by
    /// `core/tests/top_level_vs_block.rs`'s sandbox-parity tests).  Panics on
    /// a static (parse/type) failure: every call site below treats the
    /// return as a successful [`ToolResult`], never handling
    /// [`Outcome::Static`] itself.
    fn run_once(shell: &mut ral_core::Shell, cmd: &str) -> ToolResult {
        let (emit, _rx) = crate::bus::dummy_emitter();
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
        let (tmp, path_str) = scratch_file("view-tag", "alpha.txt", "alpha\n");
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
    /// The middle command fails, so the line fails; `pre_x` must be defined
    /// and `post_y` must not be.  Locks in the install-on-Error rule across
    /// the tool-call boundary.
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

        // Check survival directly against the live shell's scope rather than
        // with a follow-up tool call: looking up an undefined name from ral
        // elaborates to a type error (caught as Static), the wrong failure
        // shape for a check that expects one name present and the other
        // simply absent.
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
        let canon = display_no_trailing_sep(&tmp.canonicalize().unwrap_or_else(|_| tmp.clone()));
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
    /// addressable, so `edit-hash` always picks exactly one line.
    #[test]
    fn edit_window_hash_addresses_repeated_lines() {
        let mut shell = fresh_shell();
        let tmp = scratch_dir("window-edit");

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
                 edit-hash '{repeated_str}' [[hash: $rows[1][hash], line: 'FIRST']]"
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
                 edit-hash '{run_str}' [[hash: $rows[5][hash], line: 'Z']]"
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
        let tmp = scratch_dir("batch-edit");
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
                 edit-hash '{path_str}' [[hash: $rows[1][hash], line: 'X'], [hash: 'hzzzzzz', line: 'Y']]"
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
                 edit-hash '{path_str}' [[hash: $rows[1][hash], line: 'REPLACED'], [hash: $rows[2][hash], line: ''], [hash: $rows[3][hash], line: 'X\nY']]"
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

    /// A committed `edit-hash` notes what it changed on stderr, for the model:
    /// singular for one line and plural with a comma-joined list for a batch —
    /// plus a trailing warning when a replacement looks like it carries an
    /// unintended `\n`/`\t`-style escape rather than the real character.
    #[test]
    fn edit_notes_changed_lines_on_stderr() {
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("edit-note", "f.txt", "a\nb\nc\n");

        let one = run_once(
            &mut shell,
            &format!(
                "let rows = view-text '{path}' 1 4\n\
                 edit-hash '{path}' [[hash: $rows[0][hash], line: 'A']]"
            ),
        );
        assert_eq!(one.exit, 0, "single edit must succeed");
        assert_eq!(
            String::from_utf8_lossy(&one.stderr),
            format!("[EXARCH] Replaced line 1 of {path}.\n"),
            "a single-line edit notes the singular form with no warning"
        );

        let batch = run_once(
            &mut shell,
            &format!(
                "let rows = view-text '{path}' 1 4\n\
                 edit-hash '{path}' [[hash: $rows[1][hash], line: 'B'], [hash: $rows[2][hash], line: 'C\\n']]"
            ),
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(batch.exit, 0, "batch edit must succeed");
        assert_eq!(
            String::from_utf8_lossy(&batch.stderr),
            format!(
                "[EXARCH] Replaced lines 2, 3 of {path}. \
                 [WARNING: replacements contain escapes, did you mean to do that?]\n"
            ),
            "a multi-line batch notes the plural form, sorted, with an escape warning"
        );
    }

    /// `edit-replace` notes the line(s) it changed the same way `edit-hash` does,
    /// computed from where its unique match starts: a single line reads
    /// `line n`, a match spanning a real newline in `from` reads `lines n-m`.
    #[test]
    fn edit_replace_notes_changed_line_range_on_stderr() {
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("edit-replace-note", "f.txt", "x\nhello world\ny\n");
        let r = run_once(
            &mut shell,
            &format!("edit-replace '{path}' 'hello world' 'hi\\tthere'"),
        );
        assert_eq!(r.exit, 0, "edit-replace must succeed");
        assert_eq!(
            String::from_utf8_lossy(&r.stderr),
            format!(
                "[EXARCH] Replaced line 2 of {path}. \
                 [WARNING: replacements contain escapes, did you mean to do that?]\n"
            ),
            "a single-line match notes the singular form with an escape warning"
        );

        let (dir2, path2) = scratch_file("edit-replace-note-span", "g.txt", "one\ntwo\nthree\n");
        let spanning = run_once(
            &mut shell,
            &format!("edit-replace '{path2}' 'two\nthree' 'TWO\nTHREE'"),
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
        assert_eq!(
            spanning.exit, 0,
            "a match spanning a real newline must succeed"
        );
        assert_eq!(
            String::from_utf8_lossy(&spanning.stderr),
            format!("[EXARCH] Replaced lines 2-3 of {path2}.\n"),
            "a match spanning two lines notes the plural range, no escapes here"
        );
    }

    /// Regression: a witness hash that happens to read as a number must
    /// still round-trip from `view-text` into `edit-hash`. The agent copies the hash
    /// out of a `view-text` result and types it as a *bare* `edit-hash` argument, so
    /// an all-digit hash like `152347` lexes as an `Int` while the hash
    /// `edit-hash` recomputes is a `String`; the witness check then rejects a
    /// correct hash and the agent loops forever re-issuing the same edit.
    /// The leading-zero case (`012345`) is the sharper one: its integer
    /// reading drops the zero, so no string coercion inside `edit-hash` could
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
            let (tmp, path_str) = scratch_file(
                &format!("numeric-witness-{label}"),
                "fixture.txt",
                &format!("{content}\n"),
            );

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
                &format!("edit-hash '{path_str}' [[hash: {witness}, line: 'REPLACED']]"),
            );
            let after = std::fs::read_to_string(tmp.join("fixture.txt")).unwrap_or_default();
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
    /// its `Kind` — an io `Map` to `Kind::Io`, a `` `notice `` to
    /// `Kind::Notice`, a `` `card `` variant to `Kind::Card`, the `` `done ``
    /// completion event to `Kind::Done` (structured outcome kept, not just
    /// its ink), and a `` `pin ``/`` `unpin `` disposition to
    /// `Kind::Pin`/`Kind::Unpin` — and drops a junk value to `None`.  The
    /// foreground sink emits these now; the deferred sink's `deliver` mints
    /// the identical ones at the exchange boundary.
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
        // A `notice` variant → Kind::Notice, its outcome structure kept
        // beside the rendered card.
        assert!(matches!(
            decode_surface(&RalValue::Variant {
                label: "notice".into(),
                payload: Some(Box::new(RalValue::map(vec![
                    (
                        "kind".into(),
                        RalValue::Variant {
                            label: "reap".into(),
                            payload: None,
                        },
                    ),
                    ("cmd".into(), RalValue::String("sleep 10".into())),
                    ("cause".into(), RalValue::String("idle".into())),
                ]))),
            }),
            Some(Kind::Notice {
                notice: crate::bus::card::Notice::Reap { .. },
                ..
            })
        ));
        // A `card` variant → Kind::Card.
        assert!(matches!(
            decode_surface(&RalValue::Variant {
                label: "card".into(),
                payload: Some(Box::new(RalValue::list(vec![]))),
            }),
            Some(Kind::Card(_))
        ));
        // A `done` event → Kind::Done, its typed outcome kept beside the card.
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
            Some(Kind::Done {
                outcome: crate::bus::card::DoneOutcome::Ok,
                ..
            })
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
    fn model_surface_cannot_write_service_pins() {
        let (emit, rx) = crate::bus::dummy_emitter();
        let protected = Kind::Pin {
            key: SERVICES_PIN_KEY.to_string(),
            card: crate::bus::card::Card(Vec::new()),
        };
        assert!(reject_protected_pin(&protected, &emit));
        let event = rx.try_recv().expect("rejection should emit an error");
        assert!(
            matches!(event.kind, Kind::Error(msg) if msg.contains("protected service-ledger pin")),
            "expected protected-pin diagnostic"
        );

        let (emit, _rx) = crate::bus::dummy_emitter();
        let ordinary = Kind::Unpin {
            key: "tasks".into(),
        };
        assert!(!reject_protected_pin(&ordinary, &emit));
    }

    /// The `InboxDeferred` always posts a deferred worker's batch as a
    /// `Post::Surface` stamped with the root id and its own construction-time
    /// generation — including a batch flushed after a `/clear` advanced the
    /// registry past it. Staleness is not this sink's call: it is decided at
    /// the consuming edge (`Agent::admits`), exactly as a stale `AgentResult`
    /// is, so the sink itself neither checks nor withholds.
    #[test]
    fn inbox_deferred_always_pushes_stamped_with_its_birth_generation() {
        let registry = AgentRegistry::new();
        let inbox = Inbox::new();
        let (tx, _rx) = channel();
        let emit = Emitter::with_mailbox(tx, 7, inbox.mailbox());
        let deferred = deferred_sink(&emit, 7, &registry);
        let born = registry.generation();

        // A fresh batch reaches the inbox, stamped with the root id (7) and
        // the generation live at construction.
        deferred.deliver(vec![ral_core::serial::FOValue::Unit]);
        match inbox.next_item() {
            Some(crate::bus::Item::Surface { id, generation, .. }) => {
                assert_eq!(id, 7, "the batch is stamped with the root session id");
                assert_eq!(generation, born, "stamped with the sink's birth generation");
            }
            other => panic!("a delivered batch surfaces as Item::Surface, got {other:?}"),
        }

        // A `/clear` bumps the registry generation past the sink's captured
        // one, but a later flush still reaches the inbox, carrying the now-stale
        // generation for the consuming edge to reject.
        registry.clear_subtree(7);
        deferred.deliver(vec![ral_core::serial::FOValue::Unit]);
        match inbox.next_item() {
            Some(crate::bus::Item::Surface { generation, .. }) => {
                assert_eq!(
                    generation, born,
                    "the post-clear flush still carries its stale birth generation"
                );
            }
            other => panic!("a post-clear flush still reaches the inbox, got {other:?}"),
        }
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
    /// The fixture is the `python runtests.py` shape: `/bin/sh` forks a
    /// `sleep` grandchild that holds the stdout pipe open, then blocks in
    /// `wait`.  The standalone external leads its own process group rather
    /// than inheriting the parent's, so the watchdog's cancel path can
    /// SIGTERM (then SIGKILL) the whole group by pgid; `child.kill()`ing
    /// only the `/bin/sh` leader by pid would leave the orphaned `sleep`
    /// holding the pipe open and the stdout-pump `drain()` join blocked for
    /// the full timeout window instead of returning at the 2 s wall.
    #[cfg(unix)]
    #[test]
    fn timeout_kills_external_subprocess_tree() {
        let mut shell = fresh_shell();
        let (emit, _rx) = crate::bus::dummy_emitter();
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
            if rustix::process::test_kill_process(rustix::process::Pid::from_raw(gc_pid).unwrap())
                .is_err()
            {
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
        let (emit, _rx) = crate::bus::dummy_emitter();
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

    /// A failing external command surfaces the seam's rendering exactly
    /// once — one `error:` prefix, never a host-side re-render on top — with
    /// the command's true exit code as the tool exit (the engine computed it
    /// once; the host must not collapse it to 1) and the recovery tip,
    /// which fires only for a command's own non-zero exit.
    #[cfg(unix)]
    #[test]
    fn command_exit_renders_once_with_true_code_and_tip() {
        let mut shell = fresh_shell();
        let (emit, _rx) = crate::bus::dummy_emitter();
        let r = match run_shell_direct(
            &mut shell,
            &Capabilities::root(),
            "/bin/sh -c 'exit 3'",
            10,
            &emit,
        ) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        };
        assert_eq!(r.exit, 3, "the command's true exit code is the tool exit");
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert_eq!(
            stderr.matches("error:").count(),
            1,
            "the seam's rendering appears exactly once; stderr was: {stderr}"
        );
        assert!(
            stderr.contains("status 3"),
            "the rendering names the true status; stderr was: {stderr}"
        );
        assert!(
            stderr.contains("recovery:"),
            "a command exit carries the recovery tip; stderr was: {stderr}"
        );
    }

    /// A raised error — `fail`, not an external command's exit — carries no
    /// command-exit recovery tip: the tip's advice (read the captured
    /// output as data) is about tools that speak through exit codes, and
    /// would be noise on an explicit failure.
    #[test]
    fn raised_error_carries_no_recovery_tip() {
        let mut shell = fresh_shell();
        let (emit, _rx) = crate::bus::dummy_emitter();
        let r = match run_shell_direct(&mut shell, &Capabilities::root(), "fail 7", 10, &emit) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        };
        assert_eq!(r.exit, 7, "the raised error's status is the tool exit");
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert_eq!(
            stderr.matches("error:").count(),
            1,
            "one rendering; stderr was: {stderr}"
        );
        assert!(
            !stderr.contains("recovery:"),
            "a raised error is not a command exit; stderr was: {stderr}"
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
        let (emit, _rx) = crate::bus::dummy_emitter();
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
            if rustix::process::test_kill_process(rustix::process::Pid::from_raw(gc_pid).unwrap())
                .is_err()
            {
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
        let (emit, _rx) = crate::bus::dummy_emitter();
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
        assert_ne!(r.exit, 0, "a cancelled run must not report success");
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
            if rustix::process::test_kill_process(rustix::process::Pid::from_raw(gc_pid).unwrap())
                .is_err()
            {
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
    /// `edit-hash`.  The hashes come from `view-text`, reading the whole file so
    /// its handles match what `edit-hash` recomputes — no hash is ever read by eye.
    /// A regex `re-replace` turns
    /// each `[TODO]` into `[DONE]` in place.  This is the sweep example in
    /// `data/ral.md`, kept honest by running it.
    #[test]
    fn programmatic_todo_sweep_rewrites_every_match() {
        let mut shell = fresh_shell();
        let tmp = scratch_dir("todo-sweep");
        std::fs::write(
            tmp.join("a.txt"),
            "alpha [TODO] one\nplain\nbeta [TODO] two\n",
        )
        .expect("write a");
        std::fs::write(tmp.join("b.txt"), "gamma\ndelta [TODO] three\n").expect("write b");
        let tmp_str = display_no_trailing_sep(&tmp);

        let src = format!(
            r"cd '{tmp_str}'
let hits = grep-files #'\[TODO\]'#
let files = nub !{{map {{ |h| $h[file] }} $hits}}
each {{ |f|
    let lc = line-count $f
    let rows = view-text $f 1 $[$lc + 1]
    let mine = filter {{ |h| equal $h[file] $f }} $hits
    edit-hash $f !{{map {{ |h|
        [ hash: $rows[$[$h[line] - 1]][hash], line: !{{re-replace #'\[TODO\]'# '[DONE]' $h[text]}} ]
    }} $mine}}
}} $files
return !{{length $hits}}"
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

    /// `edit-replace` (`data/agent.ral`) — the read/`string-replace`/write idiom
    /// collapsed into one call, kept honest by running it. A unique match
    /// rewrites the file; 0 or >1 matches error and leave it untouched.
    #[test]
    fn edit_replace_replaces_unique_match_and_rejects_ambiguity() {
        let mut shell = fresh_shell();
        let (tmp, file_str) = scratch_file(
            "edit-replace",
            "config.txt",
            "USE_OPENCV := 0\nUSE_LEVELDB := 1\n",
        );

        let r = run_once(
            &mut shell,
            &format!("edit-replace '{file_str}' 'USE_OPENCV := 0' 'USE_OPENCV := 1'"),
        );
        assert_eq!(
            r.exit,
            0,
            "a unique match must rewrite the file; stderr was: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(tmp.join("config.txt")).expect("read after edit"),
            "USE_OPENCV := 1\nUSE_LEVELDB := 1\n"
        );

        let r = run_once(
            &mut shell,
            &format!("edit-replace '{file_str}' 'USE_MISSING := 0' 'x'"),
        );
        assert_ne!(r.exit, 0, "0 matches must error, not write");
        assert!(
            String::from_utf8_lossy(&r.stderr).contains("not found"),
            "stderr should explain the 0-match failure, got {:?}",
            String::from_utf8_lossy(&r.stderr)
        );

        let r = run_once(&mut shell, &format!("edit-replace '{file_str}' ':=' '='"));
        assert_ne!(
            r.exit, 0,
            "a repeated match must error, not guess which one"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.join("config.txt")).expect("read after failed edit"),
            "USE_OPENCV := 1\nUSE_LEVELDB := 1\n",
            "a rejected batch must leave the file untouched"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `edit-replace` writes through core's atomic door and surfaces the SAME
    /// structural `write` io event a committed `>` raises: the old and new
    /// snapshots ride as `old_bytes`/`new_bytes`, so the write card renders a
    /// whole-file diff.  The forensic record is the effect itself (a
    /// committed write to PATH), not only its rendered diff — and going
    /// through the shared write door is what preserves the target's
    /// mode/symlink/durability across the edit.
    #[test]
    fn edit_replace_surfaces_a_write_io_event_with_diff() {
        use crate::bus::card::{IoEvent, WriteMode, WriteOutcome, io_card};
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("edit-replace-io", "b", "hello\nworld\n");

        let (r, kinds) = run_capturing(
            &mut shell,
            &format!("edit-replace '{path}' 'world' 'friend'"),
        );
        let wrote = std::fs::read_to_string(dir.join("b")).ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            r.exit,
            0,
            "edit-replace must succeed; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(
            wrote.as_deref(),
            Some("hello\nfriend\n"),
            "the edit committed to disk"
        );

        let ios = io_events(&kinds);
        assert_eq!(
            ios.len(),
            1,
            "edit-replace raises exactly one write io event, got {ios:?}"
        );
        assert_eq!(
            ios[0],
            &IoEvent::Write {
                path,
                mode: WriteMode::Write,
                outcome: WriteOutcome::Committed,
                new_bytes: Some(b"hello\nfriend\n".to_vec()),
                old_bytes: Some(b"hello\nworld\n".to_vec()),
            },
            "old/new snapshots ride the write event so the card can diff them"
        );
        let card = io_card(ios[0]);
        assert!(
            card.has_diff(),
            "the write card renders a diff, not a plain listing; got {card:?}"
        );
    }

    /// `edit-replace` writes through core's mode-preserving atomic door, so
    /// editing an executable file leaves its `0o755` mode intact — a plain
    /// temp-file-then-rename persist would narrow it to the temp file's own
    /// `0o600`, stripping the exec bit off a script.
    #[cfg(unix)]
    #[test]
    fn edit_replace_preserves_the_target_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("edit-replace-mode", "run.sh", "#!/bin/sh\necho old\n");
        std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod the fixture executable");

        let r = run_once(&mut shell, &format!("edit-replace '{path}' 'old' 'new'"));
        let mode = std::fs::metadata(dir.join("run.sh")).map(|m| m.permissions().mode() & 0o777);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            r.exit,
            0,
            "edit-replace must succeed; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(
            mode.ok(),
            Some(0o755),
            "the executable mode must survive the edit, not narrow to 0o600"
        );
    }

    /// Drive one tool call through `run_shell` with a real bus `Emitter`,
    /// returning the result alongside every `Kind` event captured off the
    /// channel.  The end-to-end coverage harness: it exercises the whole
    /// `core surface → decode_surface → Kind` path the io-door tests below
    /// assert on, hoisted so they share one wiring rather than re-threading
    /// the channel each time.
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
    fn io_events(kinds: &[crate::bus::Kind]) -> Vec<&crate::bus::card::IoEvent> {
        kinds
            .iter()
            .filter_map(|k| match k {
                Kind::Io { event, .. } => Some(event),
                _ => None,
            })
            .collect()
    }

    /// Fresh, empty per-test temp dir named after `tag`.  Any stale dir from
    /// a prior run is removed first.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("exarch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// Write `body` into a fresh per-test temp dir and return both the dir and
    /// the display path of the file inside it (no trailing separator), the
    /// fixture shape the redirect/exec coverage tests need.
    fn scratch_file(tag: &str, name: &str, body: &str) -> (std::path::PathBuf, String) {
        let dir = scratch_dir(tag);
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
        use crate::bus::card::IoEvent;
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
            &IoEvent::Read { path },
            "the one io event is a read of the redirect path"
        );
    }

    /// Coverage — the WRITE door end-to-end: a bare `to-string "x" > b`
    /// commits an atomic write through one `>` redirect, so exactly one
    /// `Kind::Io` carrying a committed `Write` event reaches the bus.
    #[test]
    fn bare_write_redirect_surfaces_one_committed_write_card() {
        use crate::bus::card::{IoEvent, WriteMode, WriteOutcome};
        let mut shell = fresh_shell();
        // A fresh dir with no fixture file: the write creates the target.
        let dir = scratch_dir("cov-write");
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
                path,
                mode: WriteMode::Write,
                outcome: WriteOutcome::Committed,
                // The committed content rides as `new_bytes`, seeding the write
                // card's content preview.
                new_bytes: Some(b"x".to_vec()),
                // `b` is a fresh path — nothing existed to diff against.
                old_bytes: None,
            },
            "the one io event is a committed write of the redirect path"
        );
    }

    /// Coverage — the WRITE door overwriting an *existing* file: the atomic
    /// recipe leaves the target untouched until the rename, so core reads it
    /// for free and threads it through as `old_bytes` alongside the usual
    /// `new_bytes` preview.  The card layer turns that pair into a whole-file
    /// diff — the same `Mark::Diff` `edit-hash`/`edit-replace` surface explicitly, here
    /// for any `>` redirect with no builtin required.
    #[test]
    fn bare_write_redirect_over_existing_file_surfaces_a_diff_card() {
        use crate::bus::card::{IoEvent, WriteMode, WriteOutcome, io_card};
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("cov-write-diff", "b", "hello\nworld\n");

        let (r, kinds) = run_capturing(
            &mut shell,
            &format!("to-string \"hello\\nfriend\\n\" > '{path}'"),
        );
        let wrote = std::fs::read_to_string(dir.join("b")).ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            r.exit,
            0,
            "the write redirect must succeed; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(
            wrote.as_deref(),
            Some("hello\nfriend\n"),
            "the write committed to disk"
        );

        let ios = io_events(&kinds);
        assert_eq!(
            ios.len(),
            1,
            "a bare `to-string > b` raises exactly one io card, got {ios:?}"
        );
        assert_eq!(
            ios[0],
            &IoEvent::Write {
                path,
                mode: WriteMode::Write,
                outcome: WriteOutcome::Committed,
                new_bytes: Some(b"hello\nfriend\n".to_vec()),
                // `b` pre-existed: the atomic commit reads it before the
                // rename and threads it through as the diff's "before" side.
                old_bytes: Some(b"hello\nworld\n".to_vec()),
            },
            "old_bytes carries the pre-existing content, new_bytes the committed one"
        );

        let card = io_card(ios[0]);
        assert!(
            card.has_diff(),
            "overwriting an existing file renders a diff card, not a write-preview listing; got {card:?}"
        );
    }

    /// Coverage — the WRITE door overwriting an existing file too large to
    /// diff safely: core's `old_snapshot_for_diff` gates on both sides
    /// fitting its read cap (64KiB), so a bigger pre-existing file must not
    /// produce a partial, misleading diff — `old_bytes` stays absent and the
    /// card falls back to the plain listing preview, exactly as a brand-new
    /// write would.
    #[test]
    fn bare_write_redirect_over_oversized_existing_file_falls_back_to_listing() {
        use crate::bus::card::{IoEvent, WriteOutcome, io_card};
        let mut shell = fresh_shell();
        // Comfortably past core's 64KiB (65536-byte) read cap.
        let big = "x".repeat(70_000);
        let (dir, path) = scratch_file("cov-write-oversized", "b", &big);

        let (r, kinds) = run_capturing(&mut shell, &format!("to-string 'short' > '{path}'"));
        let wrote = std::fs::read_to_string(dir.join("b")).ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            r.exit,
            0,
            "the write redirect must succeed; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(
            wrote.as_deref(),
            Some("short"),
            "the write committed to disk"
        );

        let ios = io_events(&kinds);
        assert_eq!(
            ios.len(),
            1,
            "a bare `to-string > b` raises exactly one io card, got {ios:?}"
        );
        match ios[0] {
            IoEvent::Write {
                outcome, old_bytes, ..
            } => {
                assert_eq!(*outcome, WriteOutcome::Committed);
                assert!(
                    old_bytes.is_none(),
                    "an oversized pre-existing file must not be diffed"
                );
            }
            other => panic!("expected a Write event, got {other:?}"),
        }

        let card = io_card(ios[0]);
        assert!(
            !card.has_diff(),
            "an oversized pre-existing file falls back to the listing preview; got {card:?}"
        );
    }

    /// Coverage — the EXEC door end-to-end: a bare external command raises
    /// exactly one `Kind::Io` carrying an `Exec` event with the resolved argv
    /// and exit status.  `/usr/bin/true` is the deterministic, always-present
    /// image the core suite already leans on.
    #[cfg(unix)]
    #[test]
    fn bare_external_surfaces_one_exec_card() {
        use crate::bus::card::{ExecOutcome, IoEvent};
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
    /// exec card, like `grep-files` and `edit-hash`.
    #[cfg(unix)]
    #[test]
    fn view_is_a_helper_not_an_exec_image() {
        use crate::bus::card::IoEvent;
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
        use crate::bus::card::{ExecOutcome, IoEvent};
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
            &IoEvent::Read { path },
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
        use crate::bus::card::{IoEvent, io_card};
        let event = IoEvent::Write {
            path: "b.rs".into(),
            mode: crate::bus::card::WriteMode::Append,
            outcome: crate::bus::card::WriteOutcome::Committed,
            new_bytes: None,
            old_bytes: None,
        };
        let card = io_card(&event);
        let kind = Kind::Io { event, card };
        let rec = crate::agent::transcript::event_record(7, 3, &kind).expect("an io event records");

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

    // ── `user_json` ──────────────────────────────────────────────────────
    //
    // The reply projection's policy table, one case per `FOValue` variant —
    // deliberately **not** `FOValue`'s own `serde` encoding (internally
    // tagged, floats by bits), which would break the promise that a string
    // reply stays a raw JSON string and a structured one stays ordinary
    // JSON.

    #[test]
    fn user_json_unit_is_null() {
        assert_eq!(super::user_json(&FOValue::Unit), serde_json::Value::Null);
    }

    #[test]
    fn user_json_bool_and_int_are_scalars() {
        assert_eq!(
            super::user_json(&FOValue::Bool { value: true }),
            serde_json::json!(true)
        );
        assert_eq!(
            super::user_json(&FOValue::Int { value: -7 }),
            serde_json::json!(-7)
        );
    }

    #[test]
    fn user_json_finite_float_is_a_json_number() {
        assert_eq!(
            super::user_json(&FOValue::Float { value: 1.5 }),
            serde_json::json!(1.5)
        );
    }

    /// JSON has no representation for NaN or ±∞; each crosses as its own
    /// named string rather than silently becoming `null`.
    #[test]
    fn user_json_non_finite_floats_become_named_strings() {
        assert_eq!(
            super::user_json(&FOValue::Float { value: f64::NAN }),
            serde_json::json!("NaN")
        );
        assert_eq!(
            super::user_json(&FOValue::Float {
                value: f64::INFINITY
            }),
            serde_json::json!("Infinity")
        );
        assert_eq!(
            super::user_json(&FOValue::Float {
                value: f64::NEG_INFINITY
            }),
            serde_json::json!("-Infinity")
        );
    }

    #[test]
    fn user_json_string_passes_through_raw() {
        assert_eq!(
            super::user_json(&FOValue::String { value: "hi".into() }),
            serde_json::json!("hi")
        );
    }

    #[test]
    fn user_json_bytes_become_a_base64_string() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"hi");
        assert_eq!(
            super::user_json(&FOValue::Bytes {
                value: b"hi".to_vec()
            }),
            serde_json::Value::String(encoded)
        );
    }

    #[test]
    fn user_json_list_becomes_an_array() {
        let v = FOValue::List {
            items: vec![FOValue::Int { value: 1 }, FOValue::Int { value: 2 }],
        };
        assert_eq!(super::user_json(&v), serde_json::json!([1, 2]));
    }

    #[test]
    fn user_json_map_becomes_an_object() {
        let v = FOValue::Map {
            entries: vec![("a".into(), FOValue::Int { value: 1 })],
        };
        assert_eq!(super::user_json(&v), serde_json::json!({"a": 1}));
    }

    #[test]
    fn user_json_variant_with_payload_becomes_a_label_keyed_object() {
        let v = FOValue::Variant {
            label: "some".into(),
            payload: Some(Box::new(FOValue::Int { value: 3 })),
        };
        assert_eq!(super::user_json(&v), serde_json::json!({"some": 3}));
    }

    #[test]
    fn user_json_payload_less_variant_becomes_the_bare_label_string() {
        let v = FOValue::Variant {
            label: "none".into(),
            payload: None,
        };
        assert_eq!(super::user_json(&v), serde_json::json!("none"));
    }
}
