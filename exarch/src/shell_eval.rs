//! In-process ral evaluation against a persistent `Shell`.
//!
//! Each tool call is a top-level run under a pushed capabilities frame, with
//! stdout and stderr captured into buffers rather than streamed: the model
//! reads them back from history, the user sees only what the rail renders.
//! There is no source-level `grant { … }` around the model's body — the
//! boundary is `eval_top_level` plus the pushed frame, not surface syntax the
//! model could evade.

pub mod builtins;
pub mod skill;
pub mod tools;

use crate::agent::transcript::Transcript;
use crate::bus::card::{
    done_card, io_card, value_to_card, value_to_done, value_to_io, value_to_pin,
};
use crate::bus::{AgentId, AgentState, Emitter, Kind, Mailbox, Post};
use crate::fleet::registry::AgentRegistry;
use base64::Engine;
use ral_core::Value as RalValue;
use ral_core::serial::FOValue;
use ral_core::types::DeferredSink;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Idle bound of the lease armed on every detached `spawn` worker, renewed by
/// any `poll`/`await`/`race`: an abandoned worker is reaped, a babysat one
/// lives.  `pub(crate)` so the `/resources` fold reads the same constant its
/// lease rows describe.
pub(crate) const DETACHED_WORKER_CEILING: Duration = Duration::from_hours(1);

/// Absolute backstop of the same lease, measured from spawn: observation
/// renews idleness, never age.
pub(crate) const DETACHED_WORKER_BACKSTOP: Duration = Duration::from_hours(24);

/// Admission cap on concurrently *running* workers per agent, enforced by core
/// at the spawn door.  A durable service holds a seat; a settled entry
/// lingering under retention does not.
pub(crate) const LIVE_WORKER_CAP: usize = 64;

/// Birth budget on `detach` per session shell.
///
/// Deliberately not [`LIVE_WORKER_CAP`]: a detach occupies no seat, so a cap
/// counting live work cannot bound it, and it needs a bound of its own
/// precisely because nothing later reclaims it.  Reset by `/clear`, which
/// reboots the shell.
pub const DETACH_BIRTH_BUDGET: u64 = 16;

/// Retention bound, in ral calls, on a settled worker's unclaimed result,
/// counted from the per-call epoch sweep in `Agent::run_shell` that first
/// observes it settled.
pub(crate) const SETTLED_WORKER_RETENTION: u64 = 256;

/// Idle bound, in committed ral calls, on the binding-lease ledger armed on
/// every agent shell: a top-level name unused this long is pruned at the next
/// ready boundary.  Shares its figure with [`SETTLED_WORKER_RETENTION`] — one
/// ral-call clock, two ledgers reading it for their own idle policy.
pub(crate) const BINDING_IDLE_CALLS: u64 = 256;

/// Soft threshold on a session-scope install's `Value::shallow_size`: meeting
/// it queues a `LargeBindingNotice`, a nudge toward a file path over captured
/// bytes, never an eviction.
pub(crate) const LARGE_BINDING_BYTES: u64 = 1024 * 1024;

/// The prelude baked into this binary at build time by `build.rs`.
pub static PRELUDE: ral_core::boot::BakedPrelude = ral_core::baked_prelude!();

/// A successful tool run, kept in named pieces so `agent::digest::render` can
/// clip each section against its own cap and one oversized stream cannot crowd
/// out the others.
pub struct ToolResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub value: Option<String>,
    pub exit: i32,
}

/// What `run_shell` produces.  `Static` is a parse or type failure: ariadne
/// text the seam already formatted, with no sections, so it is clipped whole.
pub enum Outcome {
    Ran(ToolResult),
    Static(String),
}

/// One mirrored pin.  The bus and viewport carry plain [`Kind::Pin`]; this
/// exists only inside the agent's session mirror.
#[derive(Clone, Debug)]
pub struct PinDigest {
    pub(crate) card: crate::bus::card::Card,
}

impl PinDigest {
    pub(crate) fn new(card: crate::bus::card::Card) -> Self {
        Self { card }
    }
}

/// A shared, session-owned register of pinned-state digests, written as
/// `` `pin ``/`` `unpin `` flow past the live surface sink.
///
/// Pins otherwise flow straight through the session to the frontend, so
/// this mirror is the only way the boundary nudge can name them; `None`
/// (tests, any path with no nudge layer) disables it.
pub type PinDigests = Arc<Mutex<std::collections::BTreeMap<String, PinDigest>>>;

/// Reserved register key for the durable-service ledger — one card listing
/// every live service, authored only by `Agent::reconcile_service_pins` against
/// the live worker registry.  A model may read it, never write or clear it.
pub(crate) const SERVICES_PIN_KEY: &str = "services";

pub(crate) fn is_service_pin(key: &str) -> bool {
    key == SERVICES_PIN_KEY
}

/// Decode one surfaced `Value` into the [`Kind`] it renders as — the single
/// decoder both delivery regimes share, so the live sink's events and the
/// deferred sink's later `deliver` cannot drift.
///
/// The five shapes riding the one `surface` channel are disjoint — an io event
/// is a `Map` tagged by its `io` field, the rest are distinct variant labels —
/// so the arm order below carries no meaning.  A pin is the odd one: it is
/// *state*, keyed to a register slot and overwritten in place on re-pin, not an
/// event appended to scrollback.  Anything else drops to `None`.
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

/// Decode one surfaced value and apply the protected-pin guard.  Shared by the
/// live and deferred-batch surface sinks.
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
/// detached `spawn` worker flushes its buffered batch to at completion.  Not a
/// second channel — the live sink's own vocabulary, posted through the
/// session's [`Mailbox`] as a [`Post::Surface`] to render at the next boundary.
///
/// A worker settling mid-`/clear` cannot decide its own staleness, since
/// composing the batch and pushing it are two steps a `/clear` can fall
/// between; so the sink stamps every batch with the [`AgentRegistry`]
/// generation it captured at construction and always pushes, leaving
/// `Agent::admits` to reject a stale one at the consuming edge.
struct InboxDeferred {
    mailbox: Mailbox,
    /// The **root** session's id: a spawn worker registers no tab of its own,
    /// so its cards must land in the root viewport.
    root: AgentId,
    generation: u64,
    /// `deliver` runs on the worker's own completion, with no caller to return
    /// a `Result` to, so recording here is how a dropped batch stays visible.
    /// A `Transcript` and not an `Emitter`: this sink outlives the run, and an
    /// `Emitter`'s live bus sender held that long is the daemon-task-hang shape
    /// `drain_pass` guards against.
    transcript: Transcript,
}

impl DeferredSink for InboxDeferred {
    fn deliver(&self, batch: Vec<ral_core::serial::FOValue>) {
        // Decode once, totally, at this door: the batch crosses as first-order
        // values, the inbox renders `Value`s.
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

/// Build the [`DeferredSink`] a tool run installs, over `emit`'s session inbox.
/// Core clones it into each worker's run state, so a nested `spawn` inherits it
/// and flushes at its own completion.
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

/// Evaluate `cmd` against `transport` under `caps`, capturing stdout and
/// stderr.  Everything crosses the transport seam: a `Source` `Run` out, a
/// stream of surface events drained to the bus, one terminal `Report` back.
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

    emit.emit(Kind::State(AgentState::Evaluating));

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

    // The live sink folds pins into the mirror; a deferred worker's flush
    // renders identically but never touches it.  Shared with the identity
    // path's `DeskBinding` adapter so the two cannot diverge.
    let applier = crate::fleet::desk::SurfaceApplier {
        emit: emit.clone(),
        pins: pins.cloned(),
    };
    let report = ral_core::transport::dispatch_to_report(
        transport,
        run,
        |val| applier.live(val),
        |batch| applier.deferred(batch),
        // Dead under the identity transport, where the desk installed by
        // `set_desk` answers a mid-dispatch `Shell::enquire` directly; live
        // only for a wire engine's `Event::Enquiry`.  Either way it is the
        // same `ExarchDesk::handle` that `DeskBinding` wraps.
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
            let captured = captured.unwrap_or(ral_core::Captured {
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
                            // The model needs the remedy, not the cancellation
                            // diagnostic, so this replaces the rendering
                            // wholesale; 124 is the conventional timeout exit.
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
                            // The seam already rendered the whole diagnostic
                            // against the engine's source map: print it
                            // verbatim as the REPL does, and take the status
                            // the engine computed rather than recomputing it.
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

/// Print settings for the `VALUE` section: structured values print in ral
/// surface syntax rather than detouring through JSON, so variants stay variants
/// and records stay records.
const VALUE_PRINT_PARAMS: ral_core::builtins::PrintParams = ral_core::builtins::PrintParams {
    max_width: 120,
    max_string: 72,
    max_depth: 2,
    min_quote_hashes: 1,
    quote_bytes: true,
};

/// Render a ral value as the text the `VALUE` section carries.  A top-level
/// string or byte string is a payload and passes through raw, so file windows,
/// markdown reports, and captured byte text keep their exact lines.
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

/// Project a `reply`'s first-order payload to the JSON a user-facing edge (the
/// headless `result`) reads — deliberately **not** [`FOValue`]'s own `serde`
/// impl, which is the transport encoding (internally tagged by `kind`, floats
/// as IEEE-754 bits) and would hand the user a `{"kind":"string",…}` wrapper
/// where a bare JSON string was promised.  JSON has neither non-finite floats
/// nor a byte type, so those cross as named strings and as base64.
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
    //! They hold one `Shell` across two `run_shell` calls — the harness shape
    //! exarch uses between consecutive tool calls — to pin what routing through
    //! `eval_top_level` buys: `let` bindings persist, effects before a failing
    //! line persist while those after it never ran, and `cd` persists.  The
    //! contract itself is covered in `core/tests/top_level_vs_block.rs`; these
    //! pin that `run_shell`'s wrapping does not perturb it.

    use super::*;
    use crate::bus::{Emitter, Inbox, channel};
    use crate::shell_eval::builtins;
    use ral_core::Shell;
    use ral_core::types::Capabilities;

    /// Render a path without a trailing separator.  Some hosts return
    /// `"/tmp/"` from `std::env::temp_dir()` while `Shell::cwd()` never
    /// carries one; the `len > 1` guard keeps "/" itself intact.
    fn display_no_trailing_sep(path: &std::path::Path) -> String {
        let s = path.display().to_string();
        if s.len() > 1 {
            s.trim_end_matches(std::path::MAIN_SEPARATOR).to_string()
        } else {
            s
        }
    }

    /// A `Shell` mirroring `bootstrap::boot_shell` without signal-handler
    /// installation — global, and racey under `cargo test`.
    fn fresh_shell() -> Shell {
        let mut shell = ral_core::boot::boot_shell(
            ral_core::io::TerminalState::default(),
            &PRELUDE,
            &builtins::host_surface(),
        );
        builtins::install_agent_library(&ral_core::types::Mooring::adrift(), &mut shell)
            .expect("embedded agent library");
        crate::bootstrap::seed_no_color(&mut shell);
        shell
    }

    /// One tool run through the **real** production `run_shell`, so the test
    /// path cannot drift from what a live tool call does.  The only thing this
    /// owns that production does not is the `&mut Shell`, borrowed through a
    /// throwaway `IdentityTransport` and handed back.
    fn run_shell_direct(
        shell: &mut ral_core::Shell,
        caps: &Capabilities,
        cmd: &str,
        timeout_secs: u64,
        emit: &Emitter,
    ) -> Outcome {
        // The transport owns its `Shell`, so move the live one out behind a
        // placeholder that the swap below discards.
        let taken = std::mem::replace(
            shell,
            ral_core::Shell::new(ral_core::io::TerminalState::probe_from_env().1),
        );
        let transport = ral_core::transport::IdentityTransport::new(taken);
        let outcome = run_shell(&transport, caps, cmd, timeout_secs, emit, None, None);
        // Recover the mutated shell so `let`/`cd`/binding state reaches the
        // caller's next run — the across-calls contract these tests pin.
        *shell = transport.into_shell();
        outcome
    }

    /// One tool run under `Capabilities::root()` — exarch's least-restricted
    /// default, so every source here runs without exercising the OS sandbox,
    /// which `core/tests/top_level_vs_block.rs` covers separately.  Panics on
    /// a static failure: every call site below expects a `ToolResult`.
    fn run_once(shell: &mut ral_core::Shell, cmd: &str) -> ToolResult {
        let (emit, _rx) = crate::bus::dummy_emitter();
        match run_shell_direct(shell, &Capabilities::root(), cmd, 30, &emit) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        }
    }

    /// A restrictive exarch base: the body evaluates locally under an `fs`
    /// projection, and every external command it spawns is confined
    /// per-command under the OS sandbox.
    #[cfg(unix)]
    fn projecting_caps() -> Capabilities {
        Capabilities {
            fs: Some(ral_core::types::FsPolicy {
                read_prefixes: vec![ral_core::path::NormalizedPrefix::from_surface("/")],
                write_prefixes: vec![ral_core::path::NormalizedPrefix::from_surface("/")],
                deny_paths: Vec::new(),
            }),
            ..Capabilities::root()
        }
    }

    /// `view-text` returns one record per line: its number, its witness hash,
    /// and its text.
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

    /// The second call must see the first's binding: exarch's per-call
    /// `eval_top_level` install survives the tool-call boundary.
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
        assert_eq!(
            second.value.as_deref(),
            Some("42"),
            "expected the second call's returned value to be 42"
        );
    }

    /// The install-on-Error rule across the tool-call boundary: the middle
    /// command fails, so `pre_x` is defined and `post_y` never ran.
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

        // Against the live shell's scope, not a follow-up tool call: looking
        // up an undefined name from ral elaborates to a type error and
        // surfaces as `Static`, the wrong shape for "simply absent".
        assert!(
            shell.scope_lookup("pre_x").is_some(),
            "pre-failure `let` must persist into the next tool call"
        );
        assert!(
            shell.scope_lookup("post_y").is_none(),
            "post-failure `let` never ran, must not be present"
        );
    }

    /// `logical_cwd` rides the mobile across the tool-call boundary: a `cd` in
    /// one call is observable to `cwd` in the next.
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

    /// A witness is the smallest context that makes its line unique, so two
    /// identical lines in different surroundings differ — which a bare line
    /// hash could not manage — and a line buried in a run of identical lines
    /// grows its window to the run's edge rather than going unaddressable.
    #[test]
    fn edit_window_hash_addresses_repeated_lines() {
        let mut shell = fresh_shell();
        let tmp = scratch_dir("window-edit");

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

    /// Every hash resolves against one read before anything is written, so
    /// edits cannot interfere — not even adjacent lines, which per-call edits
    /// could not touch together without one invalidating the next.  Both
    /// halves: one stale hash writes nothing, a clean batch applies in a pass.
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

    /// A committed `edit-hash` notes what it changed on stderr for the model,
    /// plus a warning when a replacement looks like it carries an unintended
    /// `\n`/`\t`-style escape rather than the real character.
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

    /// `edit-replace` notes its changed lines as `edit-hash` does, counted
    /// from where its unique match starts: `line n`, or `lines n-m` when the
    /// match spans a real newline in `from`.
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

    /// The agent types a witness as a *bare* argument, so an all-digit one
    /// lexes as an `Int` against the `String` `edit-hash` recomputes, the check
    /// rejects a correct hash, and the agent loops forever re-issuing the same
    /// edit.  The leading-zero case is sharper: its integer reading drops the
    /// zero, so only an un-numeric witness format can recover it.
    #[cfg(unix)]
    #[test]
    fn edit_accepts_numeric_witness_hash() {
        // Mirror the adaptive-context hash to search for an all-digit one.  A
        // file written "{content}\n" has lines [content, ""], shorter than a
        // ±MIN_RADIUS (5) window, so line 1 clamps to the whole file at the
        // floor radius: its witness is lh("5:0:" ++ lh(content) ++ lh("")).
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

            // Read the witness as the agent would: from `view-text`.
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

            // Feed it back as a *bare* token, the way the agent copies it.
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

    /// Every surface class round-trips to its `Kind`, structured payload kept
    /// beside the rendered card, and junk drops to `None`.  One decoder, so
    /// what the live sink emits now is what a deferred `deliver` mints later.
    #[test]
    fn decode_surface_round_trips_each_class() {
        assert!(matches!(
            decode_surface(&RalValue::map(vec![
                ("io".into(), RalValue::String("read".into())),
                ("path".into(), RalValue::String("a.rs".into())),
            ])),
            Some(Kind::Io { .. })
        ));
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
        assert!(matches!(
            decode_surface(&RalValue::Variant {
                label: "card".into(),
                payload: Some(Box::new(RalValue::list(vec![]))),
            }),
            Some(Kind::Card(_))
        ));
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
        // An *empty* card drops the slot too: a pin with nothing to show is
        // an `unpin`.
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

    /// The sink always posts, stamped with the root id and its birth
    /// generation — even after a `/clear` advanced the registry past it.
    /// Staleness is `Agent::admits`'s call at the consuming edge, so the sink
    /// itself neither checks nor withholds.
    #[test]
    fn inbox_deferred_always_pushes_stamped_with_its_birth_generation() {
        let registry = AgentRegistry::new();
        let inbox = Inbox::new();
        let (tx, _rx) = channel();
        let emit = Emitter::with_mailbox(tx, 7, inbox.mailbox());
        let deferred = deferred_sink(&emit, 7, &registry);
        let born = registry.generation();

        deferred.deliver(vec![ral_core::serial::FOValue::Unit]);
        match inbox.next_item() {
            Some(crate::bus::Item::Surface { id, generation, .. }) => {
                assert_eq!(id, 7, "the batch is stamped with the root session id");
                assert_eq!(generation, born, "stamped with the sink's birth generation");
            }
            other => panic!("a delivered batch surfaces as Item::Surface, got {other:?}"),
        }

        // A `/clear` bumps the registry past the sink's captured generation.
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

    /// `bootstrap::seed_no_color`'s suppression reaches spawned commands
    /// through the environment, not just the host's own rendering.
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

    /// A timeout is a hard wall-clock bound, and the *whole* spawned tree must
    /// be dead after it — including a grandchild the direct child forked off.
    ///
    /// The fixture is the `python runtests.py` shape: `/bin/sh` forks a `sleep`
    /// grandchild holding the stdout pipe open, then blocks in `wait`.  A
    /// standalone external leads its own process group, so cancel tears the
    /// group down by pgid; killing the leader by pid would leave the orphan
    /// holding the pipe and the stdout pump's `drain()` join blocked.
    #[cfg(unix)]
    #[test]
    fn timeout_kills_external_subprocess_tree() {
        let mut shell = fresh_shell();
        let (emit, _rx) = crate::bus::dummy_emitter();
        // The leader blocks in `wait`, holding the call open unless the
        // timeout tears the group down; the grandchild prints its pid so the
        // test can prove it was reaped.
        let cmd = "/bin/sh -c 'sleep 30 & echo $!; wait'";
        let t0 = std::time::Instant::now();
        let r = match run_shell_direct(&mut shell, &Capabilities::root(), cmd, 2, &emit) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        };
        let elapsed = t0.elapsed();

        assert!(
            elapsed.as_secs() < 10,
            "timeout must bound wall-clock: returned after {elapsed:?} (sleep was 30s)"
        );
        assert_eq!(
            r.exit, 124,
            "a timed-out call reports the timeout exit code"
        );

        // `kill(pid, 0)` returns ESRCH once the grandchild is reaped.  Poll to
        // absorb the window between the group SIGKILL and the kernel reaping.
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

    /// `timeout_kills_external_subprocess_tree`'s fixture, asserted on the
    /// surfaced message rather than the process tree.  The elapsed budget and
    /// the `timeout_secs` name are load-bearing for steering the model, so
    /// they must stay in the text.
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

    /// The seam's rendering surfaces exactly once — no host-side re-render on
    /// top — carrying the command's true exit code rather than a host
    /// collapse to 1, plus the tip that fires only for a command's own exit.
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

    /// A raised error carries no recovery tip: its advice — read the captured
    /// output as data — is about tools that speak through exit codes, and is
    /// noise on an explicit `fail`.
    #[test]
    fn raised_error_carries_no_recovery_tip() {
        let mut shell = fresh_shell();
        let (emit, _rx) = crate::bus::dummy_emitter();
        let r = match run_shell_direct(
            &mut shell,
            &Capabilities::root(),
            "fail [status: 7]",
            10,
            &emit,
        ) {
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

    /// The same timeout must tear down a sandbox-confined eval child.  The
    /// parent is blocked in IPC while the helper runs the body, so cooperative
    /// cancel polling cannot reach it: the tree must be killed out of band.
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

    /// The eval-layer half of the registry cascade: cancelling the durable root
    /// `AgentRegistry` holds per agent (`Shell::cancel_handle`) unwinds an
    /// in-flight `run_shell` and tears down its tree.  A 30 s budget, so only
    /// the cancel can explain a fast return, over a `fork_session` child — the
    /// sub-agent shape, which publishes no process signal slots, leaving this
    /// handle the only way to stop it.
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
            // Let the eval reach the blocking external wait, then cancel from
            // outside — the registry cascade's move.
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
        // Two shapes, both the unwind under test: blocked in the child wait,
        // teardown SIGTERMs the group and the signal death is the statement
        // error; between statements, `check` raises Explicit as "cancelled".
        assert_ne!(r.exit, 0, "a cancelled run must not report success");
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert!(
            stderr.contains("cancelled") || stderr.contains("SIGTERM"),
            "the unwind surfaces the cancel or the torn-down child; stderr was: {stderr}"
        );

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

    /// A `Bytes` field renders to the model as lossy UTF-8, not the decimal
    /// array `to-json` uses for data round-trips: captured diagnostics — a
    /// job's or `audit` node's `stderr` — are byte-typed but text by intent,
    /// and the model must read the message, not a wall of codes.
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

    /// The bulk-edit pipeline end to end, entirely in ral: `grep-files` finds
    /// every `[TODO]`, the hits fold per file, and each file's lines are
    /// rewritten in one atomic `edit-hash`.  The hashes come from a whole-file
    /// `view-text`, so they match what `edit-hash` recomputes — no witness is
    /// ever read by eye.
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

    /// `edit-replace` (defined in `data/agent.ral`) collapses the
    /// read/`string-replace`/write idiom into one call: a unique match rewrites
    /// the file, and 0 or >1 matches error rather than guess.
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

    /// `edit-replace` goes through core's atomic write door, so it raises the
    /// same structural `write` event a committed `>` does — old and new
    /// snapshots riding along for the card to diff — and inherits that door's
    /// mode, symlink, and durability preservation.
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

    /// Editing an executable leaves its `0o755` intact: a plain
    /// temp-file-then-rename would narrow it to the temp file's own `0o600`,
    /// stripping the exec bit off a script.
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

    /// One tool call through `run_shell` over a real bus `Emitter`, returning
    /// the result and every `Kind` off the channel — the whole
    /// `core surface → decode_surface → Kind` path the io-door tests assert on.
    fn run_capturing(shell: &mut Shell, cmd: &str) -> (ToolResult, Vec<crate::bus::Kind>) {
        let (tx, rx) = channel();
        let emit = Emitter::new(tx, 0);
        let result = match run_shell_direct(shell, &Capabilities::root(), cmd, 30, &emit) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static (parse/type) failure: {s}"),
        };
        // Drop the emitter so the channel disconnects and `try_recv` drains to
        // empty rather than blocking.
        drop(emit);
        let kinds: Vec<crate::bus::Kind> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|ev| ev.kind)
            .collect();
        (result, kinds)
    }

    /// The `IoEvent`s carried by the captured `Kind::Io` events, in order,
    /// dropping the card composed beside each.
    fn io_events(kinds: &[crate::bus::Kind]) -> Vec<&crate::bus::card::IoEvent> {
        kinds
            .iter()
            .filter_map(|k| match k {
                Kind::Io { event, .. } => Some(event),
                _ => None,
            })
            .collect()
    }

    /// Fresh, empty per-test temp dir named after `tag`; any stale dir from a
    /// prior run is removed first.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("exarch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// Write `body` into a fresh per-test temp dir, returning the dir and the
    /// display path of the file inside it.
    fn scratch_file(tag: &str, name: &str, body: &str) -> (std::path::PathBuf, String) {
        let dir = scratch_dir(tag);
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write scratch fixture");
        let disp = display_no_trailing_sep(&path);
        (dir, disp)
    }

    /// The READ door end to end: one `<` redirect raises exactly one `Read`
    /// event, and no exec card — `from-string` is a builtin, not an image.
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

    /// The WRITE door end to end: one `>` redirect commits an atomic write and
    /// raises exactly one committed `Write` event.
    #[test]
    fn bare_write_redirect_surfaces_one_committed_write_card() {
        use crate::bus::card::{IoEvent, WriteMode, WriteOutcome};
        let mut shell = fresh_shell();
        // No fixture file: the write creates the target.
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
                new_bytes: Some(b"x".to_vec()),
                // A fresh path: nothing existed to diff against.
                old_bytes: None,
            },
            "the one io event is a committed write of the redirect path"
        );
    }

    /// Overwriting an *existing* file: the atomic recipe leaves the target
    /// untouched until the rename, so core reads it for free and threads it
    /// through as `old_bytes`.  The card layer turns the pair into a whole-file
    /// diff, so any `>` redirect gets one with no builtin required.
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
                // Read before the rename: the diff's "before" side.
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

    /// Overwriting a file too large to diff safely: `old_snapshot_for_diff`
    /// gates on both sides fitting its 64KiB read cap, so rather than render a
    /// partial and misleading diff it withholds `old_bytes` and the card falls
    /// back to the listing preview a brand-new write gets.
    #[test]
    fn bare_write_redirect_over_oversized_existing_file_falls_back_to_listing() {
        use crate::bus::card::{IoEvent, WriteOutcome, io_card};
        let mut shell = fresh_shell();
        // Comfortably past the 64KiB read cap.
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

    /// The EXEC door end to end: a bare external raises exactly one `Exec`
    /// event, carrying the resolved argv and exit status.
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

    /// `view-text` reads its path in Rust, below the ral line, so like
    /// `grep-files` and `edit-hash` it surfaces its own single READ card and no
    /// exec card at all.
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

    /// `cat < a` is two operations: the redirect's READ, then the EXEC over
    /// that stdin.  The read comes first because the door announces eagerly on
    /// install, before the body it feeds runs.
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

    /// Code loading is not turn-time data I/O: `source` and `use` read through
    /// `std::fs` below the redirect frame, so they raise no io card.  Only the
    /// loaded file's own effects would surface, and this one is pure bindings.
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

    /// `transcript::event_record` is the *operational* view: it keeps the raw
    /// structural effect the rendered card erases, and drops the card itself,
    /// which is a presentation and belongs to the TUI's `user.log`.
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
        // The mode/outcome enums cross as snake_case strings.
        assert_eq!(rec["event"]["io"], "write");
        assert_eq!(rec["event"]["path"], "b.rs");
        assert_eq!(rec["event"]["mode"], "append");
        assert_eq!(rec["event"]["outcome"], "committed");
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
    // The reply projection's policy table, one case per `FOValue` variant.

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

    /// Each crosses as its own named string rather than silently as `null`.
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
