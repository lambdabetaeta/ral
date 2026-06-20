//! One top-level turn lifted into core, behind one host entry.
//!
//! A turn is the unit a host runs over a persistent [`Shell`]: clear the
//! signal state, compile and typecheck the source against the live
//! session, install the turn's dynamic frame, evaluate the compiled
//! computation, and classify the [`Settled<Value>`] into a transport
//! status. The hosts that drive this — the interactive REPL, exarch's tool
//! evaluator, batch — all go through one entry,
//! [`Shell::run_turn`](crate::Shell::run_turn), and diverge only on the
//! policy carried in its [`TurnRequest`](crate::TurnRequest): where the byte
//! streams go, whether a capability frame is pushed, the wall and detached
//! limits, and which lifecycle hooks fire.
//!
//! This module owns the spine `run_turn` orchestrates: [`compile_turn`]
//! (parse/typecheck), [`build_turn`] (materialise the [`TurnState`]), and
//! [`run_compiled`] (install, hook, evaluate, classify). The whole turn-local
//! part of a [`Shell`] is one field — [`TurnState`] — so installing a turn is
//! a single swap: [`TurnGuard`] moves the new frame into `shell.turn`, runs,
//! and restores the previous frame on `Drop`, even when the evaluation unwinds
//! (a host may catch a worker panic and continue the session on the same
//! `Shell`). The IO regime is a sum of two cases: under `Inherit` the new
//! frame's byte sinks are cloned from the ambient session streams; under
//! `Capture` they are the fresh set the host reads back. The swap is total in
//! both cases — only the seed differs.

use crate::evaluator::eval_top_level;
use crate::io::{Io, Sink, Source};
use crate::process::{ForegroundCancelSlot, ForegroundScope, RootCancelSlot};
use crate::syntax::parser::ParseError;
use crate::typecheck::TypeError;
use crate::types::{
    Break, Capabilities, Escape, LocationCursor, Settled, Shell, SurfaceSink, TurnState, Value,
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
    _fg: ForegroundCancelSlot,
    _root: RootCancelSlot,
    shell: &'s mut Shell,
    saved: TurnState,
}

impl<'s> TurnGuard<'s> {
    /// Swap `next` into `shell.turn`, publish the installed frame's
    /// foreground scope and the session's durable root into the
    /// signal-reachable slots, and hold the displaced frame for restoration
    /// on `Drop`.
    fn install(shell: &'s mut Shell, next: TurnState) -> Self {
        let saved = std::mem::replace(&mut shell.turn, next);
        let _fg = crate::process::publish_foreground(shell.turn.cancel.as_scope());
        let _root = crate::process::publish_durable_root(shell.session.root.as_scope());
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
/// on the persistent session, so it has no liveness role.
pub(crate) fn build_turn(
    shell: &Shell,
    capture: Option<(Sink, Sink)>,
    stdin: Source,
    terminal_access: crate::types::TerminalAccess,
    foreground: ForegroundScope,
    detached_ceiling: Option<std::time::Duration>,
    surface: Option<SurfaceSink>,
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
        cancel: foreground,
        loc: LocationCursor::default(),
        detached_ceiling,
        terminal_access,
    }
}

/// Clear signal state, compile and typecheck `src` against the live session.
/// `Ok((comp, single_command))` on a clean compile; `Err(diagnostics)` on a
/// parse or type failure (the turn never reaches evaluation). The
/// `single_command` flag is harvested here because the host needs it to render
/// runtime errors after `comp` is consumed.
pub(crate) fn compile_turn(
    shell: &mut Shell,
    src: &str,
) -> Result<(Arc<crate::ir::Comp>, bool), StaticDiagnostics> {
    crate::process::clear();

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
    let outcome = compile_and_typecheck(src, schemes);
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

/// Install the built turn state, fire the lifecycle hooks around evaluation
/// under `capabilities`, compute the transport status, and tear the frame down
/// before returning `(result, status)`. The `TurnGuard` restores the prior
/// frame on `Drop`, even on unwind. Caller (`Shell::run_turn`) owns IO
/// materialisation, limit arming, and `timed_out`/capture classification.
pub(crate) fn run_compiled<'a>(
    shell: &mut Shell,
    comp: Arc<crate::ir::Comp>,
    next: TurnState,
    script_name: &str,
    src: &str,
    capabilities: Capabilities,
    mut lifecycle: Box<dyn TurnLifecycle + 'a>,
) -> (Settled<Value>, i32) {
    let mut guard = TurnGuard::install(shell, next);
    let shell = guard.shell_mut();
    shell.install_root_context(script_name.to_string(), src);

    lifecycle.pre_exec(shell, src);

    let result = shell.with_capabilities(capabilities, |s| eval_top_level(&comp, s));

    let status = eval_status(&result, shell);

    lifecycle.post_exec(shell, src, status);

    drop(guard);

    (result, status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::ByteBuffer;
    use std::sync::{Arc, Mutex};

    use crate::driver::{RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin};

    /// A capturing request under the ⊤ capability ceiling with no surface
    /// sink and no lifecycle hooks — the minimal request a host with no
    /// surface decoder or plugin policy supplies. Mirrors exarch's tool turn:
    /// foreground `Denied`, stdin `Empty`. Core mints the capture buffers and
    /// returns them in `TurnReport::Ran { captured, .. }`.
    fn capture_req<'a>() -> TurnRequest<'a> {
        TurnRequest {
            script_name: "<test>",
            caps: Capabilities::root(),
            turn_limit: None,
            detached_limit: None,
            io: TurnIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: TurnStdin::Empty,
            surface: None,
            lifecycle: Box::new(()),
        }
    }

    /// A clean turn over a simple expression settles to an `Ok` value
    /// with a zero transport status.
    #[test]
    fn clean_turn_runs_with_zero_status() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        match shell.run_turn("$[1 + 1]", capture_req()) {
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
        match shell.run_turn("let = ", capture_req()) {
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
        match shell.run_turn("$[1 + true]", capture_req()) {
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
        match shell.run_turn("exit 3", capture_req()) {
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
        match shell.run_turn("echo hi", capture_req()) {
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

        let _ = shell.run_turn(
            "$[1 + 1]",
            TurnRequest {
                surface: Some(Arc::new(())),
                ..capture_req()
            },
        );

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
        let _ = shell.run_turn(
            "exit 7",
            TurnRequest {
                lifecycle: Box::new(spy.clone()),
                ..capture_req()
            },
        );

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
        match shell.run_turn(
            "let x = 42\nreturn $x",
            TurnRequest {
                lifecycle: Box::new(CancelInPreExec),
                ..capture_req()
            },
        ) {
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
        match shell.run_turn(
            "let x = 42\nreturn $x",
            TurnRequest {
                lifecycle: Box::new(AbortInPreExec),
                ..capture_req()
            },
        ) {
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
        match shell.run_turn(
            "let x = 42\nreturn $x",
            TurnRequest {
                turn_limit: Some(std::time::Duration::from_secs(30)),
                lifecycle: Box::new(DeadlineInPreExec),
                ..capture_req()
            },
        ) {
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

        let _ = shell.run_turn(
            "echo hi",
            TurnRequest {
                io: TurnIo::Inherit,
                ..capture_req()
            },
        );

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
