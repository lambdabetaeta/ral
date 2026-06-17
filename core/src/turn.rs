//! One top-level turn lifted into core, parameterised by a frame.
//!
//! A turn is the unit a host runs over a persistent [`Shell`]: clear the
//! signal state, compile and typecheck the source against the live
//! session, install the turn's dynamic frame, evaluate the compiled
//! computation, and classify the [`Settled<Value>`] into a transport
//! status. The hosts that drive this — the interactive REPL and exarch's
//! tool evaluator — share that spine and diverge only on policy: where the
//! byte streams go, whether a capability frame is pushed, what cancel scope
//! the foreground work runs under, and which lifecycle hooks fire.
//!
//! [`eval_turn`] captures the spine once; [`TurnFrame`] gathers the policy
//! axes. The whole turn-local part of a [`Shell`] is one field —
//! [`TurnState`] — so installing a turn is a single swap: [`TurnGuard`]
//! moves the new frame into `shell.turn`, runs, and restores the previous
//! frame on `Drop`, even when the evaluation unwinds (a host may catch a
//! worker panic and continue the session on the same `Shell`). The IO regime
//! is a sum of two cases: under `Inherit` the new frame's byte sinks are
//! cloned from the ambient session streams; under `Capture` they are the
//! fresh set the host reads back. The swap is total in both cases — only the
//! seed differs.

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

/// The IO regime of a turn.
pub enum IoFrame {
    /// Run on the session's live streams: the turn's byte sinks are cloned
    /// from the ambient `shell.turn`. The interactive REPL, whose stdout is
    /// the external printer and whose stdin is the terminal.
    Inherit,
    /// Redirect the turn's streams into a fresh set the host reads back,
    /// with an optional structured-event decoder. exarch's tool capture.
    Capture {
        stdout: Sink,
        stderr: Sink,
        stdin: Source,
        surface: Option<SurfaceSink>,
    },
}

/// Optional per-turn lifecycle hooks. The REPL supplies pre/post-exec
/// plugin hooks; a host with none uses the no-op `()` impl.
pub trait TurnLifecycle {
    fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {}
    fn post_exec(&mut self, _shell: &mut Shell, _src: &str, _status: i32) {}
}

impl TurnLifecycle for () {}

/// The policy axes on which hosts differ, gathered as one type.
pub struct TurnFrame<'a> {
    pub io: IoFrame,
    /// Label for the root source context (`"<stdin>"` for the REPL,
    /// `"<tool>"` for exarch).
    pub script_name: String,
    /// The scope under which the turn's foreground work runs; installed as
    /// `shell.turn.cancel` for the turn's extent. A child of the session's
    /// durable root by construction.
    pub foreground: ForegroundScope,
    /// The capability ceiling pushed for the eval's dynamic extent via
    /// `with_capabilities`. Every turn runs under one; `Capabilities::root()`
    /// is the ⊤ element — the identity on authority, attenuating nothing
    /// (the REPL) — while a narrower profile attenuates the session's grant
    /// (exarch).
    pub capabilities: Capabilities,
    /// Lifetime ceiling for workers the turn detaches at the durable root.
    /// `None` (the interactive ral host) leaves a worker until `cancel`,
    /// root abort, or session exit; `Some(d)` (an agent host) reaps an
    /// abandoned worker `d` after it is spawned.
    pub detached_ceiling: Option<std::time::Duration>,
    pub lifecycle: Box<dyn TurnLifecycle + 'a>,
}

/// Parse/type diagnostics from a turn that never reached evaluation.
pub enum StaticDiagnostics {
    Parse(ParseError),
    Types(Vec<TypeError>),
}

/// The neutral outcome of one top-level turn. The host renders it; it
/// does not re-derive the parse/type/runtime classification.
pub enum TurnOutcome {
    /// A parse/type failure. No frame is installed and no lifecycle hooks
    /// run; `status` is 1.
    Static {
        diagnostics: StaticDiagnostics,
        status: i32,
    },
    /// A compiled turn ran. `result` carries the `Settled<Value>`;
    /// `eval_status` is computed once.
    Runtime {
        result: Settled<Value>,
        eval_status: i32,
        /// Whether the source compiled to a single command. The host needs
        /// this to render runtime errors with `format_runtime_error_auto`,
        /// since `comp` is consumed inside `eval_turn`.
        single_command: bool,
    },
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
fn build_turn(
    shell: &Shell,
    io: IoFrame,
    foreground: ForegroundScope,
    detached_ceiling: Option<std::time::Duration>,
) -> TurnState {
    let mut turn_io = shell.turn.io.try_clone().unwrap_or_else(|_| Io {
        terminal: shell.turn.io.terminal,
        interactive: shell.turn.io.interactive,
        job_control: shell.turn.io.job_control,
        ..Io::default()
    });
    let surface = match io {
        IoFrame::Inherit => shell.turn.surface.clone(),
        IoFrame::Capture {
            stdout,
            stderr,
            stdin,
            surface,
        } => {
            turn_io.stdout = stdout;
            turn_io.stderr = stderr;
            turn_io.stdin = stdin;
            surface
        }
    };
    TurnState {
        io: turn_io,
        surface,
        cancel: foreground,
        loc: LocationCursor::default(),
        detached_ceiling,
    }
}

/// Run one top-level turn of `src` against `shell` under `frame`.
///
/// Clears the signal state, compiles and typechecks against the live
/// session, and — on a clean compile — installs the frame's turn state, root
/// context, and signal-slot publications, fires the pre-exec hook, evaluates
/// under the frame's capability ceiling, computes the transport status, fires
/// the post-exec hook, and tears the frame down before returning. A parse or
/// type failure returns [`TurnOutcome::Static`] without installing any
/// context or running any hook.
pub fn eval_turn(shell: &mut Shell, src: &str, frame: TurnFrame<'_>) -> TurnOutcome {
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
        CompileOutcome::Parse(e) => {
            return TurnOutcome::Static {
                diagnostics: StaticDiagnostics::Parse(e),
                status: 1,
            };
        }
        CompileOutcome::Types(errs) => {
            return TurnOutcome::Static {
                diagnostics: StaticDiagnostics::Types(errs),
                status: 1,
            };
        }
    };

    let single_command = crate::ir::is_single_command(&comp);

    let TurnFrame {
        io,
        script_name,
        foreground,
        capabilities,
        detached_ceiling,
        mut lifecycle,
    } = frame;

    let next = build_turn(shell, io, foreground, detached_ceiling);
    let mut guard = TurnGuard::install(shell, next);
    let shell = guard.shell_mut();
    shell.install_root_context(script_name, src);

    lifecycle.pre_exec(shell, src);

    let result = shell.with_capabilities(capabilities, |s| eval_top_level(&comp, s));

    let status = eval_status(&result, shell);

    lifecycle.post_exec(shell, src, status);

    drop(guard);

    TurnOutcome::Runtime {
        result,
        eval_status: status,
        single_command,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::ByteBuffer;
    use std::sync::{Arc, Mutex};

    /// A buffer-backed capturing IO frame under the ⊤ capability ceiling
    /// with no surface sink and no lifecycle hooks — the minimal frame a
    /// host with no surface decoder or plugin policy would supply.
    fn plain_frame<'a>(shell: &Shell) -> (TurnFrame<'a>, ByteBuffer, ByteBuffer) {
        let stdout: ByteBuffer = Arc::new(Mutex::new(Vec::new()));
        let stderr: ByteBuffer = Arc::new(Mutex::new(Vec::new()));
        let frame = TurnFrame {
            io: IoFrame::Capture {
                stdout: Sink::Buffer(stdout.clone()),
                stderr: Sink::Buffer(stderr.clone()),
                stdin: Source::Terminal,
                surface: None,
            },
            script_name: "<test>".to_string(),
            foreground: shell.turn.cancel.child(),
            capabilities: Capabilities::root(),
            detached_ceiling: None,
            lifecycle: Box::new(()),
        };
        (frame, stdout, stderr)
    }

    /// A clean turn over a simple expression settles to an `Ok` value
    /// with a zero transport status.
    #[test]
    fn clean_turn_runs_with_zero_status() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        let (frame, _out, _err) = plain_frame(&shell);
        match eval_turn(&mut shell, "$[1 + 1]", frame) {
            TurnOutcome::Runtime {
                result,
                eval_status,
                ..
            } => {
                assert_eq!(eval_status, 0);
                assert!(result.is_ok(), "expected Ok, got {result:?}");
            }
            TurnOutcome::Static { .. } => panic!("clean source must not be Static"),
        }
    }

    /// Malformed source never reaches evaluation: it returns a parse
    /// diagnostic with status 1.
    #[test]
    fn parse_failure_is_static() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        let (frame, _out, _err) = plain_frame(&shell);
        match eval_turn(&mut shell, "let = ", frame) {
            TurnOutcome::Static {
                diagnostics,
                status,
            } => {
                assert_eq!(status, 1);
                assert!(
                    matches!(diagnostics, StaticDiagnostics::Parse(_)),
                    "expected a parse diagnostic"
                );
            }
            TurnOutcome::Runtime { .. } => panic!("malformed source must be Static"),
        }
    }

    /// A type error never reaches evaluation: it returns type
    /// diagnostics with status 1.
    #[test]
    fn type_failure_is_static() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        let (frame, _out, _err) = plain_frame(&shell);
        match eval_turn(&mut shell, "$[1 + true]", frame) {
            TurnOutcome::Static {
                diagnostics,
                status,
            } => {
                assert_eq!(status, 1);
                assert!(
                    matches!(diagnostics, StaticDiagnostics::Types(_)),
                    "expected type diagnostics"
                );
            }
            TurnOutcome::Runtime { .. } => panic!("ill-typed source must be Static"),
        }
    }

    /// `exit N` settles to an `Escape::Exit(N)` and reports N as the
    /// transport status.
    #[test]
    fn exit_escape_reports_code() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        let (frame, _out, _err) = plain_frame(&shell);
        match eval_turn(&mut shell, "exit 3", frame) {
            TurnOutcome::Runtime {
                result,
                eval_status,
                ..
            } => {
                assert_eq!(eval_status, 3);
                assert!(
                    matches!(result, Err(Break::Escape(Escape::Exit(3)))),
                    "expected Escape::Exit(3), got {result:?}"
                );
            }
            TurnOutcome::Static { .. } => panic!("`exit 3` must reach evaluation"),
        }
    }

    /// The frame's IO and foreground scope are torn down before
    /// `eval_turn` returns: `shell.turn.cancel` is the pre-turn scope,
    /// not the frame's, and the surface sink is restored to its prior
    /// value. Cancelling the *installed* foreground scope witnesses the
    /// swap — the pre-turn scope is a sibling, so restoring it leaves
    /// `shell.turn.cancel` uncancelled.
    #[test]
    fn frame_restores_state_on_return() {
        let _slot_guard = crate::process::signal::SLOT_SERIAL.lock().unwrap();
        let mut shell = Shell::new(Default::default());
        assert!(
            shell.turn.surface.is_none(),
            "no surface sink before the turn"
        );
        assert!(!shell.turn.cancel.is_cancelled(), "pre-turn scope is live");

        let (mut frame, _out, _err) = plain_frame(&shell);
        let installed = frame.foreground.clone();
        if let IoFrame::Capture { surface, .. } = &mut frame.io {
            *surface = Some(Arc::new(|_v: Value| {}));
        }

        let _ = eval_turn(&mut shell, "$[1 + 1]", frame);

        // Cancelling the scope the frame installed must not reach the
        // restored `shell.turn.cancel`: the teardown swapped the pre-turn
        // sibling back in.
        installed.cancel(crate::process::CancelCause::Explicit);
        assert!(
            !shell.turn.cancel.is_cancelled(),
            "foreground scope must be restored to the pre-turn sibling, not the frame's scope"
        );
        assert!(
            shell.turn.surface.is_none(),
            "surface sink must be restored to its pre-turn value"
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
        let stdout: ByteBuffer = Arc::new(Mutex::new(Vec::new()));
        let stderr: ByteBuffer = Arc::new(Mutex::new(Vec::new()));
        let frame = TurnFrame {
            io: IoFrame::Capture {
                stdout: Sink::Buffer(stdout),
                stderr: Sink::Buffer(stderr),
                stdin: Source::Terminal,
                surface: None,
            },
            script_name: "<test>".to_string(),
            foreground: shell.turn.cancel.child(),
            capabilities: Capabilities::root(),
            detached_ceiling: None,
            lifecycle: Box::new(spy.clone()),
        };

        let _ = eval_turn(&mut shell, "exit 7", frame);

        assert!(*spy.pre.lock().unwrap(), "pre_exec must fire");
        assert_eq!(
            *spy.post_status.lock().unwrap(),
            Some(7),
            "post_exec must see the computed transport status"
        );
    }

    /// `eval_turn` publishes the installed foreground scope into the
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
        let stdout: ByteBuffer = Arc::new(Mutex::new(Vec::new()));
        let stderr: ByteBuffer = Arc::new(Mutex::new(Vec::new()));
        let frame = TurnFrame {
            io: IoFrame::Capture {
                stdout: Sink::Buffer(stdout),
                stderr: Sink::Buffer(stderr),
                stdin: Source::Terminal,
                surface: None,
            },
            script_name: "<test>".to_string(),
            foreground: shell.turn.cancel.child(),
            capabilities: Capabilities::root(),
            detached_ceiling: None,
            lifecycle: Box::new(CancelInPreExec),
        };

        match eval_turn(&mut shell, "let x = 42\nreturn $x", frame) {
            TurnOutcome::Runtime {
                result,
                eval_status,
                ..
            } => {
                assert_eq!(
                    eval_status, 130,
                    "a foreground cancel observed mid-eval reports 130"
                );
                assert!(
                    matches!(result, Err(Break::Error(_))),
                    "the cancel must unwind into a Break::Error, got {result:?}"
                );
            }
            TurnOutcome::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// `eval_turn` publishes the durable root into the signal-reachable
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
        let stdout: ByteBuffer = Arc::new(Mutex::new(Vec::new()));
        let stderr: ByteBuffer = Arc::new(Mutex::new(Vec::new()));
        let frame = TurnFrame {
            io: IoFrame::Capture {
                stdout: Sink::Buffer(stdout),
                stderr: Sink::Buffer(stderr),
                stdin: Source::Terminal,
                surface: None,
            },
            script_name: "<test>".to_string(),
            foreground: shell.turn.cancel.child(),
            capabilities: Capabilities::root(),
            detached_ceiling: None,
            lifecycle: Box::new(AbortInPreExec),
        };

        match eval_turn(&mut shell, "let x = 42\nreturn $x", frame) {
            TurnOutcome::Runtime {
                result,
                eval_status,
                ..
            } => {
                assert_eq!(
                    eval_status, 130,
                    "a root abort observed mid-eval reports 130"
                );
                assert!(
                    matches!(result, Err(Break::Error(_))),
                    "the abort must unwind into a Break::Error, got {result:?}"
                );
            }
            TurnOutcome::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// Under the `Inherit` regime the turn runs on a clone of the session's
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

        let frame = TurnFrame {
            io: IoFrame::Inherit,
            script_name: "<test>".to_string(),
            foreground: shell.turn.cancel.child(),
            capabilities: Capabilities::root(),
            detached_ceiling: None,
            lifecycle: Box::new(()),
        };

        let _ = eval_turn(&mut shell, "echo hi", frame);

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
