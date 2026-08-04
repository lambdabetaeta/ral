//! One top-level run, lifted into core behind one host entry.
//!
//! A run is the unit a host evaluates over a persistent [`Shell`]. The REPL,
//! exarch's tool evaluator and batch all enter through [`Shell::run`], which
//! resolves the [`Run`]'s [`Program`] — source text or a registered hook — then
//! drives [`compile_run`], [`build_run`] and [`run_framed`] to a
//! [`Settled<Value>`] and a transport status.
//!
//! The frame splits by mutability. What a run fixes once is a [`Mooring`], an
//! owned local every callee borrows, so the stack unwinding restores an outer
//! run's. What changes — byte streams, root file, call-site register — is taken
//! on loan through [`IoLoan`], which restores on `Drop` even on unwind, since a
//! host may catch a worker panic and carry the same `Shell` on.

use crate::io::{Io, Sink, Source};
use crate::process::{CancelCause, ForegroundScope};
use crate::source::{FileId, Span};
use crate::syntax::parser::ParseError;
use crate::transport::{Program, Run};
use crate::typecheck::TypeError;
use crate::types::{
    Break, Capabilities, DeferredSink, Desk, Escape, Mooring, Nursery, NurseryGuard, Settled,
    Shell, SurfaceSink, TerminalPolicy, Value,
};
use crate::{CompileOutcome, compile_and_typecheck};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Optional per-run lifecycle hooks; a host with none uses the `()` impl.
///
/// Every production host does: the live implementors are all in tests. A hook
/// holds the run's [`Mooring`], which is what lets it surface, enquire, or
/// nest a run under the run it is hooking.
pub trait RunLifecycle {
    fn pre_exec(&mut self, _mooring: &Mooring, _shell: &mut Shell, _src: &str) {}
    fn post_exec(&mut self, _mooring: &Mooring, _shell: &mut Shell, _src: &str, _status: i32) {}
}

impl RunLifecycle for () {}

/// Parse/type diagnostics from a run that never reached evaluation.
pub enum StaticDiagnostics {
    Parse(ParseError),
    Types(Vec<TypeError>),
    /// A host-level error that stopped the run before it started: hook not
    /// found, non-ground argument, and the like.
    Host(crate::types::Error),
}

// ── The run entry: one synchronous, runtime-agnostic host seam ──────────────
//
// Hosts describe *policy*; core owns *resources*. The reduction primitive
// behind the door is crate-private, so no host can start an unframed
// evaluation that would foreground or capture against a stale frame.

/// The IO regime of a run — intent, which the run doors turn into resources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunIo {
    /// Clone the run's byte sinks from the ambient `shell.io`: the REPL and
    /// batch.
    Inherit,
    /// Mint fresh stdout/stderr buffers, returned in [`RunReport::Ran`]'s
    /// `captured`: exarch's tool runs.
    Capture,
}

/// Whether a run may hand the controlling terminal to a child.
///
/// The host states it, and [`Shell::terminal_lease`] decides whether the
/// session's [`TerminalLease`](crate::process::TerminalLease) is reachable
/// from it.  `ExplicitLoan` is absent because no host can seed it — only a
/// within-run loan token raises it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestedTerminalAccess {
    Denied,
    Leased,
}

/// The byte source a run's stdin reads from — orthogonal to [`RunIo`] (the
/// *output* regime) and to [`RequestedTerminalAccess`] (foreground
/// authority).
///
/// A piped `ral -c` is `Denied` yet still reads its inherited pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStdin {
    /// The inherited fd 0 — a terminal, a pipe, or a redirected file.
    Inherit,
    /// Immediate EOF, a child's stdin wired to `/dev/null`, no fall-through to
    /// fd 0.
    Empty,
}

/// The engine door for one run: the protocol [`Run`] plus the live,
/// non-transportable handles the host lends it.
///
/// Composition, not mirroring — a field added to [`Run`] reaches the engine
/// in one declaration.
pub struct RunRequest<'a> {
    pub run: Run,
    /// Run-local sink for structured events; `None` is the identity.
    pub surface: Option<SurfaceSink>,
    /// Session-lived destination a settling worker delivers its surface batch
    /// to; `None` leaves it reachable only through `await`/`race`.
    pub deferred: Option<Arc<dyn DeferredSink>>,
    /// Run-local enquiry desk. Same-thread children inherit it, as they do the
    /// nursery below; deferred workers get neither.
    pub desk: Option<Desk>,
    /// Run-local nursery for engine-side session forks.
    pub nursery: Option<Nursery>,
    pub lifecycle: Box<dyn RunLifecycle + 'a>,
}

/// The byte streams captured under [`RunIo::Capture`], carried verbatim onto
/// the protocol [`Report`](crate::transport::Report).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// One flat result the host matches once. `captured`/`timed_out` live on
/// `Ran`, where they mean something — a `Static` run never ran.
pub enum RunReport {
    /// The run never reached evaluation; the host renders the diagnostics and
    /// treats it as status 1.
    Static { diagnostics: StaticDiagnostics },
    /// `single_command` and `root` together pick the runtime-error rendering.
    Ran {
        result: Settled<Value>,
        status: i32,
        single_command: bool,
        root: FileId,
        captured: Option<Captured>,
        timed_out: bool,
    },
}

/// Bind the host's requested `wall` to this run's foreground scope, so the
/// deadline reaches every frame nested under it.
fn arm_wall(
    wall: std::time::Duration,
    foreground: &crate::process::ForegroundScope,
) -> crate::process::Deadline {
    crate::process::arm_lifetime(foreground.as_scope().clone(), wall)
}

/// The message of a recovered panic payload, for either string shape.
fn panic_text(payload: &dyn std::any::Any) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&'static str>().map(|s| (*s).into()))
        .unwrap_or_else(|| "non-string payload".into())
}

impl Shell {
    /// Run one whole [`Run`] synchronously and report it — the run door for a
    /// host with no run in hand and nothing to cancel it with but the session.
    ///
    /// Completion is *this call returning*, never a channel disconnecting: a
    /// worker may hold a clone of the surface sink forever without keeping the
    /// run alive. It is also the durability boundary — a panic anywhere in the
    /// run restores the [`Mobile`](crate::types::Mobile) checkpointed at entry,
    /// so the shell rolls itself back and no snapshot crosses the host seam.
    pub fn run(&mut self, req: RunRequest<'_>) -> RunReport {
        let anchor = self.session.anchor.clone();
        self.run_under(&anchor, req)
    }

    /// Run `req` with its frame under a scope the host minted with
    /// [`Shell::run_cancel_handle`] rather than under the session anchor — the
    /// door a host takes when it must be able to cancel this run from another
    /// thread, including before the run has begun.  [`Shell::run`] is this door
    /// with the anchor.
    pub fn run_under(&mut self, under: &ForegroundScope, req: RunRequest<'_>) -> RunReport {
        // A run is the extent of ral's picture of `PATH`: whatever happened to
        // the filesystem between runs is admitted here, and within a run a
        // walk is paid once per name.  Here and not in `enter` or `dispatch`,
        // because a nested run lives *inside* an outer run's extent —
        // forgetting there would throw the memo away for exactly the scripts
        // whose bodies enter builtins, which is where it pays most.
        crate::path::forget_located_commands();
        self.enter(under, req)
    }

    /// Run `req` with its frame a child of `parent` rather than of the session
    /// anchor — the door a builtin body or lifecycle hook uses, both holding the
    /// enclosing run's mooring. Nesting is what makes the cancel tree the runs'
    /// LIFO extent: the nested run observes the interrupt its outer run already
    /// carries, and the outer run's wall reaches into the nest.
    pub fn run_nested(&mut self, parent: &Mooring, req: RunRequest<'_>) -> RunReport {
        self.enter(&parent.cancel, req)
    }

    /// Both doors' body, so that the durability wrapper is the only way
    /// through either.
    fn enter(&mut self, under: &ForegroundScope, req: RunRequest<'_>) -> RunReport {
        let checkpoint = self.mobile.clone();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.dispatch(under, req))) {
            Ok(report) => report,
            Err(payload) => {
                self.mobile = checkpoint;
                RunReport::Static {
                    diagnostics: StaticDiagnostics::Host(crate::types::Error::new(
                        format!("run panicked: {}", panic_text(payload.as_ref())),
                        101,
                    )),
                }
            }
        }
    }

    /// Resolve the run's [`Program`] and hand it to the framed scaffold, with
    /// this run's foreground frame minted under `under`.
    fn dispatch(&mut self, under: &ForegroundScope, mut req: RunRequest<'_>) -> RunReport {
        match req.run.program {
            Program::Source(ref src) => {
                // The two session ledgers' clock: one tick per source dispatch
                // whether or not it goes on to compile, so a failed run ages
                // their scratch without renewing it. Both no-op when unarmed.
                self.local.bindings.tick();
                self.local.workers.tick_epoch();

                // Armed *before* compiling, so the limit bounds compile and
                // typecheck too: `compile_run`'s `process::clear` touches only
                // the signal escalation count, never the reaper. The guard
                // disarms on drop, so an early `Static` return leaves no entry.
                let foreground = self.durable_root().foreground(under);
                let wall = req.run.wall.map(|d| arm_wall(d, &foreground));

                let (comp, single_command, root) =
                    match compile_run(self, src, &req.run.script_name) {
                        Ok(parts) => parts,
                        Err(diagnostics) => return RunReport::Static { diagnostics },
                    };

                // Reaching evaluation is what renews a lease. Gated so an
                // unarmed host skips the walk over referenced names.
                if self.local.bindings.armed() {
                    self.local
                        .bindings
                        .renew(crate::ir::referenced_names(&comp));
                }

                self.run_built(req, foreground, wall, single_command, root, |m, s| {
                    crate::evaluator::eval_top_level(&comp, m, s)
                })
            }
            Program::Hook { ref name, ref args } => {
                let Some(hook) = self.mobile.context.hooks.get(name).cloned() else {
                    return RunReport::Static {
                        diagnostics: StaticDiagnostics::Host(crate::types::Error::new(
                            format!("hook '{name}' is not registered"),
                            1,
                        )),
                    };
                };

                // The host conveys data, not closures: hook args are
                // first-order by type (`FOValue`).
                let args: Vec<Value> = args.iter().cloned().map(Value::from).collect();

                let foreground = self.durable_root().foreground(under);
                let wall = req.run.wall.map(|d| arm_wall(d, &foreground));

                // Capture and terminal authority are the registered hook's to
                // decide, not the dispatching host's.
                if hook.policy.capture {
                    req.run.io = RunIo::Capture;
                }
                req.run.terminal = match hook.policy.terminal {
                    TerminalPolicy::Denied => RequestedTerminalAccess::Denied,
                    TerminalPolicy::Leased => RequestedTerminalAccess::Leased,
                };

                self.run_built(req, foreground, wall, false, FileId::DUMMY, |m, s| {
                    crate::builtins::apply(&hook.binding.value, &args, m, s)
                })
            }
        }
    }

    /// The framed scaffold both program arms share, `body` being the resolved
    /// program — `eval_top_level` for source, `builtins::apply` for a hook.
    ///
    /// The [`Mooring`] is an owned local on *this* stack frame and is only ever
    /// lent onward, so an outer run's is restored by the unwinding rather than
    /// by a guard; the [`NurseryGuard`] beside it empties its nursery on the
    /// panic path too, both being locals inside `enter`'s `catch_unwind`.
    fn run_built(
        &mut self,
        req: RunRequest<'_>,
        foreground: ForegroundScope,
        wall: Option<crate::process::Deadline>,
        single_command: bool,
        root: FileId,
        body: impl FnOnce(&Mooring, &mut Self) -> Settled<Value>,
    ) -> RunReport {
        let RunRequest {
            run,
            surface,
            deferred,
            desk,
            nursery,
            lifecycle,
        } = req;

        // A hook run has no text: its program is an already-compiled value.
        let src = match &run.program {
            Program::Source(src) => src.as_str(),
            Program::Hook { .. } => "",
        };

        let (capture, capture_bufs) = match run.io {
            RunIo::Inherit => (None, None),
            RunIo::Capture => {
                let (stdout_sink, stdout_buf) = crate::io::new_buffer();
                let (stderr_sink, stderr_buf) = crate::io::new_buffer();
                (
                    Some((stdout_sink, stderr_sink)),
                    Some((stdout_buf, stderr_buf)),
                )
            }
        };

        let stdin = match run.stdin {
            RunStdin::Inherit => Source::Terminal,
            RunStdin::Empty => Source::Empty,
        };
        let terminal_access = match run.terminal {
            RequestedTerminalAccess::Leased => crate::types::TerminalAccess::Leased,
            RequestedTerminalAccess::Denied => crate::types::TerminalAccess::Denied,
        };

        let _nursery_guard = NurseryGuard(nursery.clone());
        let mooring = Mooring {
            surface,
            deferred,
            desk,
            nursery,
            cancel: foreground,
            deferred_lease: run.deferred_lease,
            worker_cap: run.worker_cap,
            terminal_access,
        };
        let next = build_run(self, capture, stdin);
        let (result, status) = run_framed(
            &mooring,
            self,
            next,
            &run.script_name,
            src,
            run.caps.clone(),
            lifecycle,
            body,
        );

        // Disarm before reading the cause: while armed, the reaper can still
        // trip in the gap between eval returning and this read, and a run that
        // finished inside its budget would be misread as timed out.
        drop(wall);

        let timed_out = mooring.cancel.cause() == Some(CancelCause::Deadline);
        let captured = capture_bufs.map(|(out, err)| Captured {
            stdout: crate::io::take_buffer(&out),
            stderr: crate::io::take_buffer(&err),
        });

        RunReport::Ran {
            result,
            status,
            single_command,
            root,
            captured,
            timed_out,
        }
    }
}

// ── The spine behind the door: compile, build, install, classify ────────────

/// The transport status of one settled run, computed once.
fn eval_status(result: &Settled<Value>, shell: &Shell) -> i32 {
    match result {
        Ok(_) => shell.mobile.control.last_status,
        Err(Break::Error(e)) => e.exit_code(),
        Err(Break::Escape(Escape::Exit(code))) => *code,
        #[cfg(unix)]
        Err(Break::Escape(Escape::Stopped { signal, .. })) => 128 + signal.number(),
    }
}

/// Holds what a run takes on loan from the shell — the byte streams,
/// `session.root_file`, `local.audit.call_site` — and restores it on `Drop`, so
/// an unwinding run leaves nothing stale on the persistent `Shell`. Everything
/// else a run installs lives on the [`Mooring`], which was never on the shell
/// and so needs no restoring.
struct IoLoan<'s> {
    shell: &'s mut Shell,
    saved: Io,
    saved_root: FileId,
    saved_site: Option<Span>,
}

impl<'s> IoLoan<'s> {
    /// The run starts with the registers cleared, not inherited: a fresh run
    /// has no call site and no root file until it registers one.
    fn install(shell: &'s mut Shell, next: Io) -> Self {
        let saved = std::mem::replace(&mut shell.io, next);
        let saved_root = std::mem::replace(&mut shell.session.root_file, FileId::DUMMY);
        let saved_site = shell.local.audit.call_site.take();
        Self {
            shell,
            saved,
            saved_root,
            saved_site,
        }
    }

    fn shell_mut(&mut self) -> &mut Shell {
        self.shell
    }
}

impl Drop for IoLoan<'_> {
    fn drop(&mut self) {
        // Swap rather than assign, so the run's own streams move into `saved`
        // and close with the guard.
        std::mem::swap(&mut self.shell.io, &mut self.saved);
        self.shell.session.root_file = self.saved_root;
        self.shell.local.audit.call_site = self.saved_site;
    }
}

/// Build the `Io` a run installs, seeded from the ambient `shell`. `stdin` is
/// installed independently of `capture`, so [`RunIo::Capture`] does not imply
/// `Source::Terminal`; terminal authority is not here at all, but on the
/// [`Mooring`] the caller builds separately.
pub(crate) fn build_run(shell: &Shell, capture: Option<(Sink, Sink)>, stdin: Source) -> Io {
    let mut run_io = shell.io.try_clone().unwrap_or_else(|_| Io {
        terminal: shell.io.terminal,
        interactive: shell.io.interactive,
        launch_role: shell.io.launch_role,
        ..Io::default()
    });
    run_io.stdin = stdin;
    if let Some((stdout, stderr)) = capture {
        run_io.stdout = stdout;
        run_io.stderr = stderr;
    }
    run_io
}

/// Clear signal state, then compile and typecheck `src` against the live
/// session.
///
/// `single_command` and the root [`FileId`] come back here because the host
/// needs both to render runtime errors once `comp` is consumed, and `root`
/// cannot be read back later: [`IoLoan`] restores `session.root_file` to
/// [`FileId::DUMMY`] on drop. The id is *peeked* before compiling so the
/// program's spans carry this run's file identity, while
/// [`Shell::install_root_context`] registers for real from [`run_framed`], once
/// a frame exists to install into — sound only because nothing in between
/// registers a source and the registry never shrinks.
pub(crate) fn compile_run(
    shell: &Shell,
    src: &str,
    name: &str,
) -> Result<(Arc<crate::ir::Comp>, bool, FileId), StaticDiagnostics> {
    crate::process::clear();
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
    let outcome = compile_and_typecheck(src, schemes, file, name);
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
    Ok((comp, single_command, file))
}

/// Install the built run state, fire the lifecycle hooks around `body` under
/// `capabilities`, compute the transport status, and tear the frame down. Both
/// program arms settle to one [`Settled<Value>`], so this spine is shared
/// verbatim; the caller keeps the mooring, the limits and the classification.
#[allow(
    clippy::too_many_arguments,
    reason = "the run spine's whole frame in one place; a carrier struct would just relay the same fields"
)]
pub(crate) fn run_framed<'a>(
    mooring: &Mooring,
    shell: &mut Shell,
    next: Io,
    script_name: &str,
    src: &str,
    capabilities: Capabilities,
    mut lifecycle: Box<dyn RunLifecycle + 'a>,
    body: impl FnOnce(&Mooring, &mut Shell) -> Settled<Value>,
) -> (Settled<Value>, i32) {
    let mut guard = IoLoan::install(shell, next);
    let shell = guard.shell_mut();
    shell.install_root_context(script_name, src);

    lifecycle.pre_exec(mooring, shell, src);

    let result = shell.with_capabilities(capabilities, |s| body(mooring, s));

    // Cancellation is sticky until the run settles, and the run settles here: a
    // recovery construct (`try`) classifies the cancellation `Break::Error`
    // like any other and may absorb it into a clean value, but the scope is
    // monotone, so this final poll re-raises it before the status is read —
    // `post_exec` and the transport both see the true verdict. Demotes only a
    // successful settle; an error or escape already carries its own.
    let result = result.and_then(|value| crate::process::check(mooring).map(|()| value));

    let status = eval_status(&result, shell);

    lifecycle.post_exec(mooring, shell, src, status);

    // Ready-boundary housekeeping — reap notices as `` `notice `` surface
    // classes, the large-binding warning onto stderr — must go out while this
    // run's frame is still installed, so each rides this run's own stream and
    // is ordered before its Report. Once `guard` drops there is no sink left.
    shell.emit_ready_boundary_notices(mooring);

    drop(guard);

    (result, status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::ByteBuffer;
    use std::sync::{Arc, Mutex};

    /// The minimal request, shaped like exarch's tool run: ⊤ ceiling, no
    /// surface, foreground `Denied`, stdin `Empty`.
    fn capture_req<'a>(src: &str) -> RunRequest<'a> {
        RunRequest {
            run: Run {
                program: Program::Source(src.into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: RunIo::Capture,
                terminal: RequestedTerminalAccess::Denied,
                stdin: RunStdin::Empty,
            },
            surface: None,
            deferred: None,
            desk: None,
            nursery: None,
            lifecycle: Box::new(()),
        }
    }

    #[test]
    fn clean_run_settles_with_zero_status() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        match shell.run(capture_req("$[1 + 1]")) {
            RunReport::Ran { result, status, .. } => {
                assert_eq!(status, 0);
                assert!(result.is_ok(), "expected Ok, got {result:?}");
            }
            RunReport::Static { .. } => panic!("clean source must not be Static"),
        }
    }

    #[test]
    fn parse_failure_is_static() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        match shell.run(capture_req("let = ")) {
            RunReport::Static { diagnostics } => {
                assert!(
                    matches!(diagnostics, StaticDiagnostics::Parse(_)),
                    "expected a parse diagnostic"
                );
            }
            RunReport::Ran { .. } => panic!("malformed source must be Static"),
        }
    }

    #[test]
    fn type_failure_is_static() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        match shell.run(capture_req("$[1 + true]")) {
            RunReport::Static { diagnostics } => {
                assert!(
                    matches!(diagnostics, StaticDiagnostics::Types(_)),
                    "expected type diagnostics"
                );
            }
            RunReport::Ran { .. } => panic!("ill-typed source must be Static"),
        }
    }

    #[test]
    fn exit_escape_reports_code() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        match shell.run(capture_req("exit 3")) {
            RunReport::Ran { result, status, .. } => {
                assert_eq!(status, 3);
                assert!(
                    matches!(result, Err(Break::Escape(Escape::Exit(3)))),
                    "expected Escape::Exit(3), got {result:?}"
                );
            }
            RunReport::Static { .. } => panic!("`exit 3` must reach evaluation"),
        }
    }

    #[test]
    fn capture_returns_stdout_bytes() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        match shell.run(capture_req("echo hi")) {
            RunReport::Ran { captured, .. } => {
                let captured = captured.expect("Capture must return buffers");
                assert!(
                    String::from_utf8_lossy(&captured.stdout).contains("hi"),
                    "captured stdout must hold the run's output, got {:?}",
                    captured.stdout
                );
            }
            RunReport::Static { .. } => panic!("`echo hi` must reach evaluation"),
        }
    }

    /// A run's frame descends from the session anchor and dies with the run, so
    /// the next top-level run hangs off the anchor and not off the last run's.
    #[test]
    fn a_settled_run_leaves_the_anchor_untouched() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let pre = shell.session.anchor.clone();
        assert!(!pre.is_cancelled(), "pre-run anchor is live");

        let _ = shell.run(RunRequest {
            surface: Some(Arc::new(())),
            ..capture_req("$[1 + 1]")
        });

        assert!(
            !shell.session.anchor.is_cancelled(),
            "a settled run must not cancel the anchor it hung off"
        );
        shell
            .session
            .anchor
            .cancel(crate::process::CancelCause::Explicit);
        assert!(
            pre.is_cancelled(),
            "the anchor must still be the pre-run scope, not the run's own child"
        );
    }

    /// Parks a fork mid-run, as a desk handler's builtin body would.
    struct ParkDuringRun(std::sync::Arc<Mutex<Option<crate::types::NurseryId>>>);
    impl RunLifecycle for ParkDuringRun {
        fn pre_exec(&mut self, mooring: &Mooring, shell: &mut Shell, _src: &str) {
            let id = shell
                .fork_into_nursery(mooring)
                .expect("a nursery is installed on this run");
            *self.0.lock().unwrap() = Some(id);
        }
    }

    /// The host's own `Nursery` clone is empty afterward: teardown reaches
    /// through the clone, not just the run's copy.
    #[test]
    fn nursery_is_emptied_at_run_teardown() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());

        let nursery = crate::types::Nursery::default();
        let parked_id: Arc<Mutex<Option<crate::types::NurseryId>>> = Arc::new(Mutex::new(None));

        let _ = shell.run(RunRequest {
            nursery: Some(nursery.clone()),
            lifecycle: Box::new(ParkDuringRun(parked_id.clone())),
            ..capture_req("$[1 + 1]")
        });

        let id = parked_id
            .lock()
            .unwrap()
            .expect("pre_exec must fork into the nursery");
        assert!(
            nursery.adopt(id).is_none(),
            "a fork parked during the run and never adopted must not survive the run's teardown"
        );
    }

    /// A worker the lease chain reaps *between* runs has no live sink until the
    /// next run installs one, so its reap surfaces as a `` `notice `` there,
    /// ordered before that run's own settling.
    #[test]
    fn ready_boundary_notice_surfaces_a_pending_worker_reap() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());

        // Never polled, under a millisecond-scale idle lease, so the background
        // lease chain reaps it quickly.
        let mut req = capture_req("spawn { sleep 10 }");
        req.run.deferred_lease = Some(crate::types::WorkerLease {
            idle: std::time::Duration::from_millis(20),
            backstop: std::time::Duration::from_secs(10),
        });
        let _ = shell.run(req);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while shell.worker_count() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the unpolled worker must be reaped within the budget"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // The first live sink of the session: until this run, the pending reap
        // has nowhere to push through.
        struct CapturingSink(Arc<Mutex<Vec<crate::serial::FOValue>>>);
        impl crate::types::EventSink for CapturingSink {
            fn emit(&self, ev: &crate::serial::FOValue) {
                self.0.lock().unwrap().push(ev.clone());
            }
        }
        let captured: Arc<Mutex<Vec<crate::serial::FOValue>>> = Arc::new(Mutex::new(Vec::new()));
        let _ = shell.run(RunRequest {
            surface: Some(Arc::new(CapturingSink(captured.clone()))),
            ..capture_req("$[1 + 1]")
        });

        let events = captured.lock().unwrap().clone();
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
            "the settled run must surface the pending reap as a `notice`, got {events:?}"
        );
    }

    #[test]
    fn lifecycle_hooks_fire_with_status() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        #[derive(Clone)]
        struct Spy {
            pre: Arc<Mutex<bool>>,
            post_status: Arc<Mutex<Option<i32>>>,
        }
        impl RunLifecycle for Spy {
            fn pre_exec(&mut self, _mooring: &Mooring, _shell: &mut Shell, _src: &str) {
                *self.pre.lock().unwrap() = true;
            }
            fn post_exec(
                &mut self,
                _mooring: &Mooring,
                _shell: &mut Shell,
                _src: &str,
                status: i32,
            ) {
                *self.post_status.lock().unwrap() = Some(status);
            }
        }

        let mut shell = Shell::new(crate::io::TerminalState::default());
        let spy = Spy {
            pre: Arc::new(Mutex::new(false)),
            post_status: Arc::new(Mutex::new(None)),
        };
        let _ = shell.run(RunRequest {
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

    /// A run reads the interrupt watermark against its own birth: one raised
    /// from `pre_exec` — the role a signal handler plays — is younger than the
    /// run's mooring, so `process::check` unwinds the eval at 130.
    #[test]
    fn facing_session_carries_an_interrupt_into_eval() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        struct CancelInPreExec;
        impl RunLifecycle for CancelInPreExec {
            fn pre_exec(&mut self, _mooring: &Mooring, _shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Interrupt);
            }
        }

        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.face_signals();
        match shell.run(RunRequest {
            lifecycle: Box::new(CancelInPreExec),
            ..capture_req("let x = 42\nreturn $x")
        }) {
            RunReport::Ran { result, status, .. } => {
                assert_eq!(
                    status, 130,
                    "a foreground cancel observed mid-eval reports 130"
                );
                assert!(
                    matches!(result, Err(Break::Error(_))),
                    "the cancel must unwind into a Break::Error, got {result:?}"
                );
            }
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// `try` classifies a cancellation like any recoverable error, so a handler
    /// that settles cleanly resets `last_status` to 0 — the run boundary must
    /// re-poll the monotone scope before the status is read, or a final
    /// `try { … } { |_| return unit }` suppresses the interrupt entirely.
    #[test]
    fn a_try_handler_cannot_settle_a_cancelled_run() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        struct CancelInPreExec;
        impl RunLifecycle for CancelInPreExec {
            fn pre_exec(&mut self, _mooring: &Mooring, _shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Interrupt);
            }
        }

        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.face_signals();
        match shell.run(RunRequest {
            lifecycle: Box::new(CancelInPreExec),
            ..capture_req("try { let x = unit\nreturn $x } { |_| return unit }")
        }) {
            RunReport::Ran { result, status, .. } => {
                assert_eq!(
                    status, 130,
                    "a try handler must not settle a cancelled run at 0"
                );
                match result {
                    Err(Break::Error(e)) => assert_eq!(
                        e.message, "interrupted",
                        "the boundary poll must report the cancel cause's own words"
                    ),
                    other => panic!("the re-poll must unwind into a Break::Error, got {other:?}"),
                }
            }
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// A nested run observes what the run it nests in observes. Both unwind at
    /// 130 though the nested frame is younger than the interrupt and deaf to it
    /// on its own account: it reads it off the outer frame it descends from.
    #[test]
    fn a_nested_run_observes_the_interrupt_through_the_run_it_nests_in() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        struct NestARun(Arc<Mutex<Option<i32>>>);
        impl RunLifecycle for NestARun {
            fn pre_exec(&mut self, mooring: &Mooring, shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Interrupt);
                match shell.run_nested(mooring, capture_req("let y = 1\nreturn $y")) {
                    RunReport::Ran { status, .. } => *self.0.lock().unwrap() = Some(status),
                    RunReport::Static { .. } => panic!("valid source must reach evaluation"),
                }
            }
        }

        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.face_signals();
        let inner = Arc::new(Mutex::new(None));
        match shell.run(RunRequest {
            lifecycle: Box::new(NestARun(inner.clone())),
            ..capture_req("let x = 42\nreturn $x")
        }) {
            RunReport::Ran { status, .. } => assert_eq!(
                status, 130,
                "the outer run must still observe the interrupt the nested run ran under"
            ),
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
        assert_eq!(
            *inner.lock().unwrap(),
            Some(130),
            "and the nested run observes it too — sharing, not shadowing"
        );
    }

    /// An aside — a separate `Shell` beside the session, as the REPL runs plugin
    /// hooks ([`Shell::join_session`]) — can neither absorb an interrupt aimed
    /// at the session's run nor withhold it. Not authority but age: the aside
    /// mints its frames under its own boot frame, so an interrupt older than all
    /// of them is unreadable from there.
    #[test]
    fn an_aside_cannot_absorb_an_interrupt_older_than_its_frames() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        struct RunAnAside(Shell, Arc<Mutex<Option<i32>>>);
        impl RunLifecycle for RunAnAside {
            fn pre_exec(&mut self, _mooring: &Mooring, _shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Interrupt);
                match self.0.run(capture_req("let y = 1\nreturn $y")) {
                    RunReport::Ran { status, .. } => *self.1.lock().unwrap() = Some(status),
                    RunReport::Static { .. } => panic!("valid source must reach evaluation"),
                }
            }
        }

        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.face_signals();
        let aside = shell.join_session();
        let hook_status = Arc::new(Mutex::new(None));
        match shell.run(RunRequest {
            lifecycle: Box::new(RunAnAside(aside, hook_status.clone())),
            ..capture_req("let x = 42\nreturn $x")
        }) {
            RunReport::Ran { status, .. } => assert_eq!(
                status, 130,
                "the interrupt must survive the aside's entry and unwind the run it was aimed at"
            ),
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
        assert_eq!(
            *hook_status.lock().unwrap(),
            Some(0),
            "while the aside's own run, born after the interrupt, is deaf to it"
        );
    }

    /// The other half: an interrupt raised *while* the aside runs does unwind
    /// it, so plugin code is interruptible for its whole life.
    #[test]
    fn an_aside_unwinds_on_an_interrupt_raised_while_it_runs() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        struct CancelInPreExec;
        impl RunLifecycle for CancelInPreExec {
            fn pre_exec(&mut self, _mooring: &Mooring, _shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Interrupt);
            }
        }

        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.face_signals();
        let mut aside = shell.join_session();
        match aside.run(RunRequest {
            lifecycle: Box::new(CancelInPreExec),
            ..capture_req("let y = 1\nreturn $y")
        }) {
            RunReport::Ran { status, .. } => assert_eq!(
                status, 130,
                "a Ctrl-C during a hook must unwind the hook it lands in"
            ),
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// [`Shell::cancel_handle`] — how a host stops a session it owns — does
    /// reach the aside beside it, the one thing that can, because the handle
    /// names the very root the aside shares.
    #[test]
    fn cancelling_a_session_by_handle_reaches_its_aside() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.face_signals();
        let mut aside = shell.join_session();

        shell
            .cancel_handle()
            .cancel(crate::process::CancelCause::Explicit);

        match aside.run(capture_req("let y = 1\nreturn $y")) {
            RunReport::Ran { result, status, .. } => {
                assert_eq!(status, 130, "the aside's run unwinds with the session");
                assert!(
                    matches!(result, Err(Break::Error(_))),
                    "the handle cancel must unwind the aside, got {result:?}"
                );
            }
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// A forked session ([`Shell::fork_session`]) does not face signals, so
    /// nothing on its runs' chains folds the ambient causes and a mid-run
    /// interrupt leaves the eval undisturbed. Its host stops it through
    /// [`Shell::cancel_handle`] instead, as the second half asserts.
    #[test]
    fn a_forked_session_is_deaf_to_the_requested_causes() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        struct CancelInPreExec;
        impl RunLifecycle for CancelInPreExec {
            fn pre_exec(&mut self, _mooring: &Mooring, _shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Interrupt);
            }
        }

        let mut trunk = Shell::new(crate::io::TerminalState::default());
        // Face the trunk, so the fork's deafness below is `fork_session`'s
        // doing rather than the minting default's.
        trunk.face_signals();
        let mut forked = trunk.fork_session();
        match forked.run(RunRequest {
            lifecycle: Box::new(CancelInPreExec),
            ..capture_req("let x = 42\nreturn $x")
        }) {
            RunReport::Ran { result, status, .. } => {
                assert_eq!(
                    status, 0,
                    "a foreground-cancel request must not reach a forked session's run"
                );
                assert!(
                    result.is_ok(),
                    "the forked session's eval completes undisturbed, got {result:?}"
                );
            }
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }

        struct CancelHandleInPreExec(crate::process::DurableRoot);
        impl RunLifecycle for CancelHandleInPreExec {
            fn pre_exec(&mut self, _mooring: &Mooring, _shell: &mut Shell, _src: &str) {
                self.0.cancel(crate::process::CancelCause::Explicit);
            }
        }
        let handle = forked.cancel_handle();
        match forked.run(RunRequest {
            lifecycle: Box::new(CancelHandleInPreExec(handle)),
            ..capture_req("let x = 42\nreturn $x")
        }) {
            RunReport::Ran { result, status, .. } => {
                assert_eq!(status, 130, "a cancelled handle unwinds the run");
                assert!(
                    matches!(result, Err(Break::Error(_))),
                    "the handle cancel must unwind into a Break::Error, got {result:?}"
                );
            }
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// A signal-facing session's durable root folds the requested shutdown cause
    /// — the role a SIGQUIT handler plays — and the run's foreground scope,
    /// being its child, observes it through the parent chain.
    #[test]
    fn facing_session_carries_a_root_abort_into_eval() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        struct AbortInPreExec;
        impl RunLifecycle for AbortInPreExec {
            fn pre_exec(&mut self, _mooring: &Mooring, _shell: &mut Shell, _src: &str) {
                crate::process::request_root_cancel(crate::process::CancelCause::RootAbort);
            }
        }

        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.face_signals();
        let report = shell.run(RunRequest {
            lifecycle: Box::new(AbortInPreExec),
            ..capture_req("let x = 42\nreturn $x")
        });
        // One-way in production, so nothing clears it: without this, every
        // later session in the test binary would inherit the abort.
        crate::process::cancel::clear_root_request();
        match report {
            RunReport::Ran { result, status, .. } => {
                assert_eq!(status, 130, "a root abort observed mid-eval reports 130");
                assert!(
                    matches!(result, Err(Break::Error(_))),
                    "the abort must unwind into a Break::Error, got {result:?}"
                );
            }
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// The hook fires the `Deadline` cause directly, pinning the `timed_out`
    /// classification without sleeping on the reaper.
    #[test]
    fn deadline_cancel_reports_timed_out() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        struct DeadlineInPreExec;
        impl RunLifecycle for DeadlineInPreExec {
            fn pre_exec(&mut self, _mooring: &Mooring, _shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Deadline);
            }
        }

        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.face_signals();
        let req = capture_req("let x = 42\nreturn $x");
        match shell.run(RunRequest {
            run: Run {
                wall: Some(std::time::Duration::from_secs(30)),
                ..req.run
            },
            lifecycle: Box::new(DeadlineInPreExec),
            ..req
        }) {
            RunReport::Ran { timed_out, .. } => {
                assert!(
                    timed_out,
                    "a Deadline foreground cancel must report timed_out"
                );
            }
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// Under `RunIo::Inherit` the guard restores the session's stdout sink to
    /// the *same* object it was before the run, and the run's output lands in
    /// it.
    #[test]
    fn inherit_leaves_session_streams_untouched() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let marker: ByteBuffer = Arc::new(Mutex::new(Vec::new()));
        shell.io.stdout = Sink::Buffer(marker.clone());

        let req = capture_req("echo hi");
        let _ = shell.run(RunRequest {
            run: Run {
                io: RunIo::Inherit,
                ..req.run
            },
            ..req
        });

        assert!(
            matches!(&shell.io.stdout, Sink::Buffer(b) if Arc::ptr_eq(b, &marker)),
            "Inherit must restore the session's stdout sink after the run"
        );
        let written = marker.lock().unwrap().clone();
        assert!(
            !written.is_empty(),
            "the run's stdout must land in the inherited sink"
        );
        assert!(
            String::from_utf8_lossy(&written).contains("hi"),
            "the inherited sink must receive the run's output"
        );
    }

    // ── Run-door durability ──────────────────────────────────────────
    //
    // A panic anywhere in the run reports as a failed run with the shell's
    // `Mobile` already rolled back to its run-entry state.

    /// Stands in for any Rust panic the evaluator can raise mid-run.
    fn builtin_panic_now(
        _args: &[crate::types::Value],
        _mooring: &Mooring,
        _shell: &mut Shell,
    ) -> crate::types::Settled<crate::types::Value> {
        panic!("run-door test: deliberate mid-eval panic");
    }

    fn scheme_panic_now(_u: &mut crate::typecheck::Unifier) -> crate::typecheck::Scheme {
        use crate::typecheck::builtins::{mk_scheme, pure, thunk};
        mk_scheme(&[], &[], &[], thunk(pure(crate::typecheck::Ty::Unit)))
    }

    static PANIC_BUILTINS_ARR: [crate::types::BuiltinEntry; 1] = [crate::types::BuiltinEntry::new(
        std::borrow::Cow::Borrowed("core-panic-now"),
        crate::typecheck::builtins::BuiltinTypeRule::Scheme(scheme_panic_now),
        "test-only: panic the evaluator mid-run.",
        crate::types::BuiltinBody::Static(builtin_panic_now),
    )];
    static PANIC_BUILTINS: &[crate::types::BuiltinEntry] = &PANIC_BUILTINS_ARR;

    /// Rollback is to the run's entry, not to session birth: the panicking run's
    /// partial binding goes, a pre-run one stays, and the shell runs on.
    #[test]
    fn panicking_run_reports_failed_and_rolls_back() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.install_builtins(PANIC_BUILTINS);

        match shell.run(capture_req("let pre_panic = 1")) {
            RunReport::Ran { result, .. } => assert!(result.is_ok()),
            RunReport::Static { .. } => panic!("the pre-run binding must evaluate"),
        }

        match shell.run(capture_req("let mid_panic = 2\ncore-panic-now")) {
            RunReport::Static {
                diagnostics: StaticDiagnostics::Host(e),
            } => {
                assert!(
                    e.message.contains("run panicked"),
                    "the report must name the panic, got {:?}",
                    e.message
                );
                assert!(
                    e.message.contains("deliberate mid-eval panic"),
                    "the report must carry the panic payload, got {:?}",
                    e.message
                );
            }
            _ => panic!("a panicking run must report Static{{Host}}"),
        }

        assert!(
            shell.scope_lookup("pre_panic").is_some(),
            "a pre-run binding must survive a later run's panic"
        );
        assert!(
            shell.scope_lookup("mid_panic").is_none(),
            "the panicking run's own partial binding must be rolled back"
        );

        match shell.run(capture_req("$pre_panic")) {
            RunReport::Ran { result, .. } => {
                assert!(
                    result.is_ok(),
                    "the next run must evaluate clean on the same shell"
                );
            }
            RunReport::Static { .. } => panic!("the healed shell must still evaluate"),
        }
    }

    /// A panic after evaluation, outside the evaluator proper: the whole run
    /// spine is inside the checkpoint, not just the eval.
    struct PanicInPostExec;
    impl RunLifecycle for PanicInPostExec {
        fn post_exec(&mut self, _mooring: &Mooring, _shell: &mut Shell, _src: &str, _status: i32) {
            panic!("run-door test: post_exec panic");
        }
    }

    #[test]
    fn lifecycle_panic_is_caught_at_the_door() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());

        match shell.run(RunRequest {
            lifecycle: Box::new(PanicInPostExec),
            ..capture_req("let post_panic = 3")
        }) {
            RunReport::Static {
                diagnostics: StaticDiagnostics::Host(e),
            } => assert!(e.message.contains("run panicked")),
            _ => panic!("a lifecycle panic must report Static{{Host}}"),
        }
        assert!(
            shell.scope_lookup("post_panic").is_none(),
            "a post-exec panic rolls the whole run back — its binding included"
        );
    }

    /// A host-supplied handler panicking mid-enquiry unwinds back through the
    /// run, so the door catches it like any other.
    struct PanickingDesk;
    impl crate::types::EnquiryDesk for PanickingDesk {
        fn enquire(
            &self,
            _req: crate::serial::FOValue,
            _cancel: &crate::process::CancelScope,
        ) -> Result<crate::serial::FOValue, crate::types::Error> {
            panic!("run-door test: desk handler panic");
        }
    }

    struct EnquireInPreExec;
    impl RunLifecycle for EnquireInPreExec {
        fn pre_exec(&mut self, mooring: &Mooring, shell: &mut Shell, _src: &str) {
            let _ = shell.enquire(mooring, crate::serial::FOValue::Unit);
        }
    }

    #[test]
    fn desk_handler_panic_is_caught_at_the_door() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());

        match shell.run(RunRequest {
            desk: Some(Arc::new(PanickingDesk)),
            lifecycle: Box::new(EnquireInPreExec),
            ..capture_req("let desk_panic = 4")
        }) {
            RunReport::Static {
                diagnostics: StaticDiagnostics::Host(e),
            } => assert!(e.message.contains("run panicked")),
            _ => panic!("a desk-handler panic must report Static{{Host}}"),
        }
        assert!(
            shell.scope_lookup("desk_panic").is_none(),
            "the enquiring run's binding must be rolled back with the rest"
        );
    }

    // ── where a runtime error says it happened ───────────────────────────

    /// The rendered runtime error of a run that must fault.
    fn rendered_fault(shell: &mut Shell, src: &str) -> String {
        let report = shell.run(capture_req(src)).into_report(shell.sources());
        let crate::transport::Report::Ran { result, .. } = report else {
            panic!("{src:?} must reach evaluation");
        };
        match result {
            Err(crate::transport::Break::Error { rendered, .. }) => rendered,
            other => panic!("{src:?} must fault, got {other:?}"),
        }
    }

    /// A lambda compiled by one run and called by the next draws its caret into
    /// the text that defined it: the registry only grows, so a run boundary
    /// costs a value nothing of its origin.
    #[test]
    fn a_lambda_faults_against_the_run_that_compiled_it() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        assert!(
            matches!(shell.run(capture_req("let boom = { |x| $undefined_name }")), RunReport::Ran { result, .. } if result.is_ok())
        );
        let rendered = crate::ansi::strip(&rendered_fault(&mut shell, "boom 1"));
        assert!(
            rendered.contains("$undefined_name }"),
            "the caret must be drawn into the defining run's text:\n{rendered}"
        );
    }

    /// A fault in text the user can already see needs no header and no caret.
    #[test]
    fn a_single_command_faulting_in_its_own_text_renders_compact() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let rendered = crate::ansi::strip(&rendered_fault(&mut shell, "no-such-command-xyz"));
        assert!(
            !rendered.contains('╭'),
            "a fault in the text on screen needs no caret:\n{rendered}"
        );
    }

    /// exarch reads `single_command` to say whether a non-zero exit aborted
    /// anything after it, so the flag must keep reporting the input's shape.
    #[test]
    fn single_command_still_reports_the_input_shape() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        for (src, shape) in [
            ("no-such-command-xyz", true),
            ("no-such-command-xyz; true", false),
        ] {
            match shell.run(capture_req(src)) {
                RunReport::Ran {
                    single_command,
                    result,
                    ..
                } => {
                    assert_eq!(single_command, shape, "{src:?} misreports its shape");
                    assert!(result.is_err(), "{src:?} must fault");
                }
                RunReport::Static { .. } => panic!("{src:?} must reach evaluation"),
            }
        }
    }
}
