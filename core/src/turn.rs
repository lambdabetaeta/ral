//! One top-level turn lifted into core, behind one host entry.
//!
//! A turn is the unit a host runs over a persistent [`Shell`]: clear the
//! signal state, fix the turn's program (compiled from source, or an
//! already-evaluated thunk applied to argument values), install the turn's
//! dynamic frame, evaluate, and classify the [`Settled<Value>`] into a
//! transport status. The hosts that drive this — the interactive REPL,
//! exarch's tool evaluator, batch — go through one door,
//! [`Shell::run_turn`](crate::Shell::run_turn), whose
//! [`Turn`](crate::transport::Turn) carries the program
//! ([`Program`](crate::transport::Program): source text or a registered
//! hook) and the conditions it runs under: where the byte streams go,
//! whether a capability frame is pushed, the wall and deferred limits, and
//! which lifecycle hooks fire.
//!
//! This module owns the spine the turn doors orchestrate: [`compile_turn`]
//! (parse/typecheck), [`build_turn`] (materialise the [`TurnState`]), and
//! [`run_framed`] (install, hook, evaluate, classify). The whole turn-local
//! part of a [`Shell`] is one field — [`TurnState`] — so installing a turn is
//! a single swap: [`TurnGuard`] moves the new frame into `shell.turn`, runs,
//! and restores the previous frame on `Drop`, even when the evaluation unwinds
//! (a host may catch a worker panic and continue the session on the same
//! `Shell`). The IO regime is a sum of two cases: under `Inherit` the new
//! frame's byte sinks are cloned from the ambient session streams; under
//! `Capture` they are the fresh set the host reads back. The swap is total in
//! both cases — only the seed differs.

use crate::io::{Io, Sink, Source};
use crate::process::{ForegroundCancelSlot, ForegroundScope, RootCancelSlot};
use crate::syntax::parser::ParseError;
use crate::typecheck::TypeError;
use crate::types::{
    Break, Capabilities, DeferredSink, Desk, Escape, LocationCursor, Settled, Shell, SurfaceSink,
    TurnState, Value,
};
use crate::{CompileOutcome, compile_and_typecheck};
use std::sync::Arc;

/// Optional per-turn lifecycle hooks. The REPL supplies pre/post-exec
/// plugin hooks; a host with none uses the no-op `()` impl.
pub trait TurnLifecycle {
    fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {}
    fn post_exec(&mut self, _shell: &mut Shell, _src: &str, _status: i32) {}
}

impl TurnLifecycle for () {}

/// Parse/type diagnostics from a turn that never reached evaluation.
pub enum StaticDiagnostics {
    Parse(ParseError),
    Types(Vec<TypeError>),
    /// A host-level error that prevented the turn from running:
    /// hook not found, non-ground argument, etc.
    Host(crate::types::Error),
}

/// The transport status of one settled turn: the success status for a
/// normal return, the error's exit code for a caught error, the requested
/// code for `exit`, and `128 + signal` for a stopped job.
fn eval_status(result: &Settled<Value>, shell: &Shell) -> i32 {
    match result {
        Ok(_) => shell.mobile.control.last_status,
        Err(Break::Error(e)) => e.exit_code(),
        Err(Break::Escape(Escape::Exit(code))) => *code,
        #[cfg(unix)]
        Err(Break::Escape(Escape::Stopped { signal, .. })) => 128 + signal.number(),
    }
}

/// Saves the previous turn frame on install and restores it on `Drop`,
/// and owns the per-turn foreground and durable-root signal-slot
/// publications for the same extent. Restoring under `Drop` rather than an
/// explicit epilogue keeps the persistent `Shell` clean even when the
/// evaluation unwinds — a host that catches a worker panic and continues the
/// session on the same `Shell` would otherwise inherit a stale cursor, an
/// already-cancelled foreground scope, and (had the turn captured IO) the
/// prior turn's buffers and a foreign surface sink.
///
/// Only the signal-facing session publishes
/// (`SessionState::publishes_signal_slots`): the slots' save/restore
/// discipline is LIFO on one thread, which concurrent sessions — a host's
/// forked sub-agents dispatching turns on their own threads — would
/// violate; and a signal must target the primary session's turn, never
/// whichever session dispatched last.  A non-publishing session's turns
/// are cancelled through its scope handles instead
/// ([`Shell::cancel_handle`](crate::types::Shell::cancel_handle)).
///
/// The slot publications point at raw flags inside the installed frame's
/// cancel scope. `Drop::drop` swaps that frame into `saved`, whose scope is
/// freed when `saved` itself drops; the slots must un-publish before that
/// happens. Rust runs `Drop::drop` first, then drops fields in declaration
/// order, so the slots — declared before `saved` — un-publish before the
/// displaced frame's scope is freed, and a signal slot never points at a
/// freed flag.
struct TurnGuard<'s> {
    // Dropped (after `Drop::drop` runs the swap) in declaration order:
    // the slots un-publish before `saved` frees the displaced frame's
    // cancel scope, so a signal slot never points at a freed flag.
    _fg: Option<ForegroundCancelSlot>,
    _root: Option<RootCancelSlot>,
    shell: &'s mut Shell,
    saved: TurnState,
}

impl<'s> TurnGuard<'s> {
    /// Swap `next` into `shell.turn`, publish the installed frame's
    /// foreground scope and the session's durable root into the
    /// signal-reachable slots (signal-facing session only), and hold the
    /// displaced frame for restoration on `Drop`.
    fn install(shell: &'s mut Shell, next: TurnState) -> Self {
        let saved = std::mem::replace(&mut shell.turn, next);
        let publish = shell.session.publishes_signal_slots;
        let _fg = publish.then(|| crate::process::publish_foreground(shell.turn.cancel.as_scope()));
        let _root =
            publish.then(|| crate::process::publish_durable_root(shell.session.root.as_scope()));
        Self {
            _fg,
            _root,
            shell,
            saved,
        }
    }

    fn shell_mut(&mut self) -> &mut Shell {
        self.shell
    }
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        // Swap the saved frame back in; the new frame moves into `saved` and
        // drops with the guard. The `_fg`/`_root` slots then un-publish in
        // field order before `saved` frees the displaced frame's cancel scope,
        // so a signal slot never points at a freed flag.
        std::mem::swap(&mut self.shell.turn, &mut self.saved);
    }
}

/// Build the [`TurnState`] a turn installs, seeded from the ambient `shell`.
///
/// `capture` is `Some` only under [`TurnIo::Capture`](crate::driver::TurnIo),
/// where the turn's byte *output* streams are redirected into host-read
/// buffers; under `Inherit` it is `None` and the ambient streams flow through
/// unchanged. `stdin` and `terminal_access` are supplied independently of the
/// output regime (the host's [`TurnStdin`](crate::driver::TurnStdin) and
/// [`RequestedTerminalAccess`](crate::driver::RequestedTerminalAccess)): stdin is
/// always installed, so `Capture` no longer implies `Source::Terminal`. The
/// `surface` is always the request's turn-local sink — it is no longer carried
/// on the persistent session, so it has no liveness role.  `deferred` is the
/// session-lived destination a deferred worker delivers its surface batch to
/// when it settles.  `desk` is the request's turn-local enquiry desk; `None`
/// outside a host that answers enquiries.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_turn(
    shell: &Shell,
    capture: Option<(Sink, Sink)>,
    stdin: Source,
    terminal_access: crate::types::TerminalAccess,
    foreground: ForegroundScope,
    deferred_lease: Option<crate::types::WorkerLease>,
    worker_cap: Option<usize>,
    surface: Option<SurfaceSink>,
    deferred: Option<Arc<dyn DeferredSink>>,
    desk: Option<Desk>,
) -> TurnState {
    let mut turn_io = shell.turn.io.try_clone().unwrap_or_else(|_| Io {
        terminal: shell.turn.io.terminal,
        interactive: shell.turn.io.interactive,
        launch_role: shell.turn.io.launch_role,
        ..Io::default()
    });
    turn_io.stdin = stdin;
    if let Some((stdout, stderr)) = capture {
        turn_io.stdout = stdout;
        turn_io.stderr = stderr;
    }
    TurnState {
        io: turn_io,
        surface,
        deferred,
        desk,
        cancel: foreground,
        loc: LocationCursor::default(),
        deferred_lease,
        worker_cap,
        terminal_access,
    }
}

/// Clear signal state, compile and typecheck `src` against the live session.
/// `Ok((comp, single_command))` on a clean compile; `Err(diagnostics)` on a
/// parse or type failure (the turn never reaches evaluation). The
/// `single_command` flag is harvested here because the host needs it to render
/// runtime errors after `comp` is consumed.
///
/// Resets the session's source registry and peeks the [`FileId`] the next
/// registration will mint *before* compiling, so the compiled program's
/// spans carry this turn's real file identity. [`Shell::install_root_context`]
/// performs the actual reset-then-register once the turn frame exists to
/// install it into ([`run_framed`]); nothing between here and there touches
/// the registry, so the id it mints always agrees with the one peeked here.
pub(crate) fn compile_turn(
    shell: &mut Shell,
    src: &str,
) -> Result<(Arc<crate::ir::Comp>, bool), StaticDiagnostics> {
    crate::process::clear();
    shell.session.sources.reset();
    let file = shell.session.sources.next_id();

    #[cfg(debug_assertions)]
    let t_seed = std::time::Instant::now();
    let schemes = shell.session_schemes();
    #[cfg(debug_assertions)]
    let n_bindings = schemes.bindings.len();
    crate::dbg_trace!(
        "shell",
        "session_schemes: {n_bindings} names in {:?}",
        t_seed.elapsed()
    );

    #[cfg(debug_assertions)]
    let t_tc = std::time::Instant::now();
    let outcome = compile_and_typecheck(src, schemes, file);
    crate::dbg_trace!(
        "shell",
        "compile_and_typecheck: {n_bindings} bindings, {} src bytes in {:?}",
        src.len(),
        t_tc.elapsed()
    );
    let comp = match outcome {
        CompileOutcome::Compiled(c) => Arc::new(c),
        CompileOutcome::Parse(e) => return Err(StaticDiagnostics::Parse(e)),
        CompileOutcome::Types(errs) => return Err(StaticDiagnostics::Types(errs)),
    };

    let single_command = crate::ir::is_single_command(&comp);
    Ok((comp, single_command))
}

/// Install the built turn state, fire the lifecycle hooks around the eval
/// `body` under `capabilities`, compute the transport status, and tear the
/// frame down before returning `(result, status)`. The `body` is the turn's
/// program: the source door evaluates a compiled [`Comp`](crate::ir::Comp)
/// (`eval_top_level`); the value door applies an already-evaluated thunk in
/// place (`builtins::apply`). Both settle to one [`Settled<Value>`], so the
/// lifecycle/capability/status spine is shared verbatim. The `TurnGuard`
/// restores the prior frame on `Drop`, even on unwind. The caller (the
/// `run_*_turn` doors) owns IO materialisation, limit arming, and the
/// `timed_out`/capture classification.
pub(crate) fn run_framed<'a>(
    shell: &mut Shell,
    next: TurnState,
    script_name: &str,
    src: &str,
    capabilities: Capabilities,
    mut lifecycle: Box<dyn TurnLifecycle + 'a>,
    body: impl FnOnce(&mut Shell) -> Settled<Value>,
) -> (Settled<Value>, i32) {
    let mut guard = TurnGuard::install(shell, next);
    let shell = guard.shell_mut();
    shell.install_root_context(script_name.to_string(), src);

    lifecycle.pre_exec(shell, src);

    let result = shell.with_capabilities(capabilities, body);

    let status = eval_status(&result, shell);

    lifecycle.post_exec(shell, src, status);

    // Push this turn's ready-boundary housekeeping — the lease chain's reap
    // notices, the large-binding warning — as `` `notice `` surface classes
    // while this turn's frame (and its surface sink) is still installed, so
    // the notice rides this turn's own surface stream, ordered before its
    // Report (`decisions/260706_enquiry-channel` §4.2). After `guard` drops
    // below, `shell.turn` reverts and there is no sink left to push through.
    shell.emit_ready_boundary_notices();

    drop(guard);

    (result, status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::ByteBuffer;
    use std::sync::{Arc, Mutex};

    use crate::driver::{RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin};
    use crate::transport::{Program, Turn};

    /// A capturing request under the ⊤ capability ceiling with no surface
    /// sink and no lifecycle hooks — the minimal request a host with no
    /// surface decoder or plugin policy supplies. Mirrors exarch's tool turn:
    /// foreground `Denied`, stdin `Empty`. Core mints the capture buffers and
    /// returns them in `TurnReport::Ran { captured, .. }`.
    fn capture_req<'a>(src: &str) -> TurnRequest<'a> {
        TurnRequest {
            turn: Turn {
                program: Program::Source(src.into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                turn_limit: None,
                deferred_lease: None,
                worker_cap: None,
                io: TurnIo::Capture,
                terminal: RequestedTerminalAccess::Denied,
                stdin: TurnStdin::Empty,
            },
            surface: None,
            deferred: None,
            desk: None,
            lifecycle: Box::new(()),
        }
    }

    /// A clean turn over a simple expression settles to an `Ok` value
    /// with a zero transport status.
    #[test]
    fn clean_turn_runs_with_zero_status() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        match shell.run_turn(capture_req("$[1 + 1]")) {
            TurnReport::Ran { result, status, .. } => {
                assert_eq!(status, 0);
                assert!(result.is_ok(), "expected Ok, got {result:?}");
            }
            TurnReport::Static { .. } => panic!("clean source must not be Static"),
        }
    }

    /// Malformed source never reaches evaluation: it returns a parse
    /// diagnostic.
    #[test]
    fn parse_failure_is_static() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        match shell.run_turn(capture_req("let = ")) {
            TurnReport::Static { diagnostics } => {
                assert!(
                    matches!(diagnostics, StaticDiagnostics::Parse(_)),
                    "expected a parse diagnostic"
                );
            }
            TurnReport::Ran { .. } => panic!("malformed source must be Static"),
        }
    }

    /// A type error never reaches evaluation: it returns type diagnostics.
    #[test]
    fn type_failure_is_static() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        match shell.run_turn(capture_req("$[1 + true]")) {
            TurnReport::Static { diagnostics } => {
                assert!(
                    matches!(diagnostics, StaticDiagnostics::Types(_)),
                    "expected type diagnostics"
                );
            }
            TurnReport::Ran { .. } => panic!("ill-typed source must be Static"),
        }
    }

    /// `exit N` settles to an `Escape::Exit(N)` and reports N as the
    /// transport status.
    #[test]
    fn exit_escape_reports_code() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        match shell.run_turn(capture_req("exit 3")) {
            TurnReport::Ran { result, status, .. } => {
                assert_eq!(status, 3);
                assert!(
                    matches!(result, Err(Break::Escape(Escape::Exit(3)))),
                    "expected Escape::Exit(3), got {result:?}"
                );
            }
            TurnReport::Static { .. } => panic!("`exit 3` must reach evaluation"),
        }
    }

    /// `TurnIo::Capture` returns the turn's stdout in `Ran::captured`.
    #[test]
    fn capture_returns_stdout_bytes() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        match shell.run_turn(capture_req("echo hi")) {
            TurnReport::Ran { captured, .. } => {
                let captured = captured.expect("Capture must return buffers");
                assert!(
                    String::from_utf8_lossy(&captured.stdout).contains("hi"),
                    "captured stdout must hold the turn's output, got {:?}",
                    captured.stdout
                );
            }
            TurnReport::Static { .. } => panic!("`echo hi` must reach evaluation"),
        }
    }

    /// The turn's foreground scope and turn-local surface are torn down
    /// before `run_turn` returns: the surface is restored to its pre-turn
    /// value, and cancelling the restored `shell.turn.cancel` reaches the
    /// pre-turn scope (proving the guard swapped the pre-turn frame back in,
    /// not the turn's internally-minted child).
    #[test]
    fn frame_restores_state_on_return() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        assert!(
            shell.turn.surface.is_none(),
            "no surface sink before the turn"
        );
        assert!(!shell.turn.cancel.is_cancelled(), "pre-turn scope is live");
        let pre = shell.foreground().clone();

        let _ = shell.run_turn(TurnRequest {
            surface: Some(Arc::new(())),
            ..capture_req("$[1 + 1]")
        });

        assert!(
            shell.turn.surface.is_none(),
            "turn-local surface must be restored to its pre-turn value"
        );
        // Cancelling the restored foreground must reach the pre-turn scope:
        // the teardown swapped the pre-turn frame back in, discarding the
        // turn's internally-minted child.
        shell
            .foreground()
            .cancel(crate::process::CancelCause::Explicit);
        assert!(
            pre.is_cancelled(),
            "foreground scope must be restored to the pre-turn scope"
        );
    }

    /// A no-op desk, just enough to install a non-`None` value on the turn.
    struct NoopDesk;
    impl crate::types::EnquiryDesk for NoopDesk {
        fn enquire(
            &self,
            req: crate::serial::FOValue,
        ) -> Result<crate::serial::FOValue, crate::types::Error> {
            Ok(req)
        }
    }

    /// The desk twin of `frame_restores_state_on_return`: the turn-local
    /// enquiry desk is torn down before `run_turn` returns, restored
    /// to its pre-turn value (`None`) exactly as `surface` is.
    #[test]
    fn desk_is_restored_to_its_pre_turn_value() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        assert!(shell.turn.desk.is_none(), "no desk before the turn");

        let _ = shell.run_turn(TurnRequest {
            desk: Some(Arc::new(NoopDesk)),
            ..capture_req("$[1 + 1]")
        });

        assert!(
            shell.turn.desk.is_none(),
            "turn-local desk must be restored to its pre-turn value"
        );
    }

    /// A settling turn's ready-boundary housekeeping surfaces the expected
    /// `` `notice `` class (`decisions/260706_enquiry-channel` §4.2): a
    /// worker the lease chain reaps between turns has no live sink to push
    /// through until the *next* turn runs one — installed here on that next
    /// turn, it captures the reap as a `notice` variant, ordered before this
    /// turn's own settling.
    #[test]
    fn ready_boundary_notice_surfaces_a_pending_worker_reap() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());

        // Spawn a worker under a millisecond-scale idle lease and never
        // poll it, so the background lease chain reaps it quickly.
        let mut req = capture_req("spawn { sleep 10 }");
        req.turn.deferred_lease = Some(crate::types::WorkerLease {
            idle: std::time::Duration::from_millis(20),
            backstop: std::time::Duration::from_secs(10),
        });
        let _ = shell.run_turn(req);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while shell.worker_count() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the unpolled worker must be reaped within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // A live sink installed only on this *next* turn: the pending reap
        // has nowhere to push through until now.
        struct CapturingSink(Arc<Mutex<Vec<crate::serial::FOValue>>>);
        impl crate::types::EventSink for CapturingSink {
            fn emit(&self, ev: &crate::serial::FOValue) {
                self.0.lock().unwrap().push(ev.clone());
            }
        }
        let captured: Arc<Mutex<Vec<crate::serial::FOValue>>> = Arc::new(Mutex::new(Vec::new()));
        let _ = shell.run_turn(TurnRequest {
            surface: Some(Arc::new(CapturingSink(captured.clone()))),
            ..capture_req("$[1 + 1]")
        });

        let events = captured.lock().unwrap();
        let saw_reap_notice = events.iter().any(|ev| {
            let crate::serial::FOValue::Variant { label, payload } = ev else {
                return false;
            };
            if label != "notice" {
                return false;
            }
            let Some(payload) = payload else { return false };
            let crate::serial::FOValue::Map { entries } = payload.as_ref() else {
                return false;
            };
            entries.iter().any(|(k, v)| {
                k == "kind"
                    && matches!(v, crate::serial::FOValue::Variant { label, .. } if label == "reap")
            })
        });
        assert!(
            saw_reap_notice,
            "the settled turn must surface the pending reap as a `notice`, got {events:?}"
        );
    }

    /// A lifecycle test double records that both hooks fired and that
    /// `post_exec` saw the computed transport status.
    #[test]
    fn lifecycle_hooks_fire_with_status() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        #[derive(Clone)]
        struct Spy {
            pre: Arc<Mutex<bool>>,
            post_status: Arc<Mutex<Option<i32>>>,
        }
        impl TurnLifecycle for Spy {
            fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {
                *self.pre.lock().unwrap() = true;
            }
            fn post_exec(&mut self, _shell: &mut Shell, _src: &str, status: i32) {
                *self.post_status.lock().unwrap() = Some(status);
            }
        }

        let mut shell = Shell::new(Default::default());
        let spy = Spy {
            pre: Arc::new(Mutex::new(false)),
            post_status: Arc::new(Mutex::new(None)),
        };
        let _ = shell.run_turn(TurnRequest {
            lifecycle: Box::new(spy.clone()),
            ..capture_req("exit 7")
        });

        assert!(*spy.pre.lock().unwrap(), "pre_exec must fire");
        assert_eq!(
            *spy.post_status.lock().unwrap(),
            Some(7),
            "post_exec must see the computed transport status"
        );
    }

    /// `run_turn` publishes the installed foreground scope into the
    /// signal-reachable slot for the eval's extent: a lifecycle hook that
    /// requests a foreground cancel (the role a signal handler plays) reaches
    /// `shell.turn.cancel`, and the trampoline's `process::check` unwinds
    /// the eval into a `Break::Error` with transport status 130.
    #[test]
    fn published_foreground_slot_carries_request_into_eval() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        struct CancelInPreExec;
        impl TurnLifecycle for CancelInPreExec {
            fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Interrupt);
            }
        }

        let mut shell = Shell::new(Default::default());
        match shell.run_turn(TurnRequest {
            lifecycle: Box::new(CancelInPreExec),
            ..capture_req("let x = 42\nreturn $x")
        }) {
            TurnReport::Ran { result, status, .. } => {
                assert_eq!(
                    status, 130,
                    "a foreground cancel observed mid-eval reports 130"
                );
                assert!(
                    matches!(result, Err(Break::Error(_))),
                    "the cancel must unwind into a Break::Error, got {result:?}"
                );
            }
            TurnReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// A forked session ([`Shell::fork_session`]) is not the signal-facing
    /// session, so its turn doors leave the signal slots untouched: a
    /// foreground-cancel request fired mid-turn (the role a signal handler
    /// plays) finds no published slot and the eval completes undisturbed.
    /// Its host stops it through [`Shell::cancel_handle`] instead — the
    /// companion assertion below cancels the handle and sees the same
    /// unwind a published slot would have produced.
    #[test]
    fn forked_session_turn_does_not_publish_signal_slots() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        struct CancelInPreExec;
        impl TurnLifecycle for CancelInPreExec {
            fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Interrupt);
            }
        }

        let trunk = Shell::new(Default::default());
        let mut forked = trunk.fork_session();
        match forked.run_turn(TurnRequest {
            lifecycle: Box::new(CancelInPreExec),
            ..capture_req("let x = 42\nreturn $x")
        }) {
            TurnReport::Ran { result, status, .. } => {
                assert_eq!(
                    status, 0,
                    "a foreground-cancel request must not reach a forked session's turn"
                );
                assert!(
                    result.is_ok(),
                    "the forked session's eval completes undisturbed, got {result:?}"
                );
            }
            TurnReport::Static { .. } => panic!("valid source must reach evaluation"),
        }

        // The host-side path: cancelling the forked session's handle unwinds
        // its next turn at the evaluator's poll, exactly as a published slot
        // would have for the signal-facing session.
        struct CancelHandleInPreExec(crate::process::DurableRoot);
        impl TurnLifecycle for CancelHandleInPreExec {
            fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {
                self.0.cancel(crate::process::CancelCause::Explicit);
            }
        }
        let handle = forked.cancel_handle();
        match forked.run_turn(TurnRequest {
            lifecycle: Box::new(CancelHandleInPreExec(handle)),
            ..capture_req("let x = 42\nreturn $x")
        }) {
            TurnReport::Ran { result, status, .. } => {
                assert_eq!(status, 130, "a cancelled handle unwinds the turn");
                assert!(
                    matches!(result, Err(Break::Error(_))),
                    "the handle cancel must unwind into a Break::Error, got {result:?}"
                );
            }
            TurnReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// `run_turn` publishes the durable root into the signal-reachable
    /// root slot for the turn's extent: a lifecycle hook that requests a
    /// root abort (the role a SIGQUIT handler plays) reaches the root, and
    /// the foreground scope — a child of the root — observes the abort
    /// through its parent chain. The trampoline's `process::check` unwinds
    /// the eval into a `Break::Error` with transport status 130.
    #[test]
    fn published_root_slot_carries_abort_into_eval() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        struct AbortInPreExec;
        impl TurnLifecycle for AbortInPreExec {
            fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {
                crate::process::request_root_cancel(crate::process::CancelCause::RootAbort);
            }
        }

        let mut shell = Shell::new(Default::default());
        match shell.run_turn(TurnRequest {
            lifecycle: Box::new(AbortInPreExec),
            ..capture_req("let x = 42\nreturn $x")
        }) {
            TurnReport::Ran { result, status, .. } => {
                assert_eq!(status, 130, "a root abort observed mid-eval reports 130");
                assert!(
                    matches!(result, Err(Break::Error(_))),
                    "the abort must unwind into a Break::Error, got {result:?}"
                );
            }
            TurnReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// A turn whose wall (`turn_limit`) is armed and whose foreground scope is
    /// cancelled with `Deadline` reports `timed_out`. Driven through a
    /// lifecycle hook that fires the deadline cause directly, so the test
    /// pins the `timed_out` classification without sleeping on the reaper.
    #[test]
    fn deadline_cancel_reports_timed_out() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        struct DeadlineInPreExec;
        impl TurnLifecycle for DeadlineInPreExec {
            fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Deadline);
            }
        }

        let mut shell = Shell::new(Default::default());
        let req = capture_req("let x = 42\nreturn $x");
        match shell.run_turn(TurnRequest {
            turn: Turn {
                turn_limit: Some(std::time::Duration::from_secs(30)),
                ..req.turn
            },
            lifecycle: Box::new(DeadlineInPreExec),
            ..req
        }) {
            TurnReport::Ran { timed_out, .. } => {
                assert!(
                    timed_out,
                    "a Deadline foreground cancel must report timed_out"
                );
            }
            TurnReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// Under `TurnIo::Inherit` the turn runs on a clone of the session's
    /// streams and the guard restores them on teardown: the session's stdout
    /// sink is the *same* object before and after the turn (its `Arc` is
    /// shared into the turn and restored after), and the turn's output lands
    /// in it.
    #[test]
    fn inherit_leaves_session_streams_untouched() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        let marker: ByteBuffer = Arc::new(Mutex::new(Vec::new()));
        shell.turn.io.stdout = Sink::Buffer(marker.clone());

        let req = capture_req("echo hi");
        let _ = shell.run_turn(TurnRequest {
            turn: Turn {
                io: TurnIo::Inherit,
                ..req.turn
            },
            ..req
        });

        assert!(
            matches!(&shell.turn.io.stdout, Sink::Buffer(b) if Arc::ptr_eq(b, &marker)),
            "Inherit must restore the session's stdout sink after the turn"
        );
        let written = marker.lock().unwrap();
        assert!(
            !written.is_empty(),
            "the turn's stdout must land in the inherited sink"
        );
        assert!(
            String::from_utf8_lossy(&written).contains("hi"),
            "the inherited sink must receive the turn's output"
        );
    }
}
