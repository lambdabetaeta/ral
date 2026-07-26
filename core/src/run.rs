//! One top-level run lifted into core, behind one host entry.
//!
//! A run is the unit a host evaluates over a persistent [`Shell`]: clear the
//! signal state, fix the run's program (compiled from source, or an
//! already-evaluated thunk applied to argument values), install the run's
//! dynamic frame, evaluate, and classify the [`Settled<Value>`] into a
//! transport status. The hosts that drive this — the interactive REPL,
//! exarch's tool evaluator, batch — go through one door, [`Shell::run`],
//! whose [`Run`] carries the program ([`Program`]: source text or a
//! registered hook) and the conditions it runs under: where the byte streams
//! go, whether a capability frame is pushed, the wall and deferred limits,
//! and which lifecycle hooks fire.
//!
//! The door and the types a host describes its policy with ([`RunIo`],
//! [`RunStdin`], [`RequestedTerminalAccess`], [`RunRequest`]) live here, and
//! behind them the spine they orchestrate: [`compile_run`] (parse/typecheck),
//! [`build_run`] (materialise the [`RunState`]), and [`run_framed`] (install,
//! hook, evaluate, classify). The whole run-local part of a [`Shell`] is one
//! field — [`RunState`] — so installing a run is a single swap: [`RunGuard`]
//! moves the new frame into `shell.run`, runs, and restores the previous
//! frame on `Drop`, even when the evaluation unwinds (a host may catch a
//! worker panic and continue the session on the same `Shell`). The IO regime
//! is a sum of two cases: under `Inherit` the new frame's byte sinks are
//! cloned from the ambient session streams; under `Capture` they are the
//! fresh set the host reads back. The swap is total in both cases — only the
//! seed differs.

use crate::io::{Io, Sink, Source};
use crate::process::{CancelCause, CancelSlot, ForegroundScope};
use crate::syntax::parser::ParseError;
use crate::transport::{Program, Run};
use crate::typecheck::TypeError;
use crate::types::{
    Break, Capabilities, DeferredSink, Desk, Escape, LocationCursor, Nursery, RunState, Settled,
    Shell, SurfaceSink, TerminalPolicy, Value,
};
use crate::{CompileOutcome, compile_and_typecheck};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Optional per-run lifecycle hooks. The REPL supplies pre/post-exec
/// plugin hooks; a host with none uses the no-op `()` impl.
pub trait RunLifecycle {
    fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {}
    fn post_exec(&mut self, _shell: &mut Shell, _src: &str, _status: i32) {}
}

impl RunLifecycle for () {}

/// Parse/type diagnostics from a run that never reached evaluation.
pub enum StaticDiagnostics {
    Parse(ParseError),
    Types(Vec<TypeError>),
    /// A host-level error that prevented the run from starting:
    /// hook not found, non-ground argument, etc.
    Host(crate::types::Error),
}

// ── The run entry: one synchronous, runtime-agnostic host seam ──────────────
//
// Hosts start an evaluation through exactly one door:
// `Shell::run(RunRequest)` runs one whole `Run` — its `Program` is
// either source text or a registered hook applied to first-order arguments
// (`Shell::register_hook` stores compiled hooks by name in the session-lived
// hook table).  It returns one flat `RunReport`.  Hosts describe *policy*
// (the protocol `Run`, `RunIo`, `SurfaceSink`, lifecycle hooks); core owns
// *resources* (`Sink`, `Source`, `RunState`, guards, buffers, signal
// slots).  Completion is the call returning — never a channel disconnecting
// — so a deferred worker holding a surface clone cannot keep a run from
// ending.  This door is the only way into evaluation: the reduction
// primitive behind it is crate-private, so a host cannot start an unframed
// evaluation that would foreground or capture against a stale frame.

/// The IO regime of a run: intent, materialised into resources by the run
/// doors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunIo {
    /// Run on the session's live streams: the run's byte sinks are cloned
    /// from the ambient `shell.run`. The interactive REPL (whose stdout is
    /// the external printer) and batch (the process streams).
    Inherit,
    /// Mint fresh stdout/stderr buffers core returns in
    /// [`RunReport::Ran`]'s `captured`. Independent of [`RunStdin`]: byte
    /// output regime and byte input source are separate choices. exarch's tool
    /// capture.
    Capture,
}

/// Whether a run may hand the controlling terminal to a child.
///
/// The host-facing half of the terminal lease: the host states the run's
/// authority, and core decides whether the session's
/// [`TerminalLease`](crate::process::TerminalLease) is reachable from it (see
/// [`Shell::terminal_lease`]). `ExplicitLoan` is deliberately absent — a host
/// cannot seed it; it is a within-run elevation a loan token raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestedTerminalAccess {
    /// No child/job foreground handoff in this run. exarch tool runs and any
    /// launch that does not own the terminal foreground.
    Denied,
    /// This run may foreground terminal-bound children. The interactive REPL
    /// and a terminal-launched script.
    Leased,
}

/// The byte source a run's stdin reads from.
///
/// Orthogonal to [`RunIo`] (the *output* regime) and to
/// [`RequestedTerminalAccess`] (foreground authority): a piped `ral -c` is
/// `Denied` foreground yet still reads its inherited pipe (`Inherit`), while an
/// exarch tool run is `Denied` *and* reads no terminal (`Empty`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunStdin {
    /// Use the session stdin source — the inherited fd 0, which may be a
    /// terminal, a pipe, or a redirected file.
    Inherit,
    /// Install an empty source: reads as immediate EOF, a child's stdin wires
    /// to `/dev/null`, and there is no fall-through to fd 0.
    Empty,
}

/// The engine door for one run: the protocol [`Run`] plus the live,
/// non-transportable handles the host lends it.
///
/// Composition, not
/// mirroring — a field added to [`Run`] crosses the seam and reaches the
/// engine in one declaration.
pub struct RunRequest<'a> {
    /// The run, exactly as it crosses (or would cross) the host seam.
    pub run: Run,
    /// The run-local structured-event sink, installed only for this run.
    /// `None` is the identity (a bare REPL). Same-thread children inherit it;
    /// deferred workers buffer into bounded deferred storage instead.
    pub surface: Option<SurfaceSink>,
    /// The session-lived destination a deferred worker delivers its surface
    /// batch to when it settles, rendered by the host at the next run
    /// boundary. `None` outside an agent host (a bare REPL): then a deferred
    /// worker's surface reaches a sink only via `await`/`race`.
    pub deferred: Option<Arc<dyn DeferredSink>>,
    /// The run-local enquiry desk, installed only for this run. `None` is
    /// the honest absence a host that answers no enquiries reports (a bare
    /// REPL, and exarch until the migration installs its desk). Same-thread
    /// children inherit it; deferred workers never receive it.
    pub desk: Option<Desk>,
    /// The run-local nursery for engine-side session forks, installed only
    /// for this run. `None` outside a host that installs one. Same-thread
    /// children inherit it; deferred workers never receive it.
    pub nursery: Option<Nursery>,
    /// Per-run lifecycle hooks; `Box::new(())` for a host with none.
    pub lifecycle: Box<dyn RunLifecycle + 'a>,
}

/// The byte streams captured under [`RunIo::Capture`], returned in
/// [`RunReport::Ran`] and carried verbatim on the protocol
/// [`Report`](crate::transport::Report).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// One flat result the host matches once. `captured`/`timed_out` live on
/// `Ran`, where they mean something — a `Static` run never ran.
pub enum RunReport {
    /// A parse/type failure: the run never reached evaluation. The host
    /// renders the diagnostics and treats the run as status 1.
    Static { diagnostics: StaticDiagnostics },
    /// A compiled run ran. `status` is the transport status computed once;
    /// `single_command` is whether the source compiled to a single command
    /// (for runtime-error rendering); `captured` is `Some` under
    /// [`RunIo::Capture`]; `timed_out` is whether the wall fired.
    Ran {
        result: Settled<Value>,
        status: i32,
        single_command: bool,
        captured: Option<Captured>,
        timed_out: bool,
    },
}

/// Resolve a run's armed wall clock from every source that can bind it:
/// the host's requested `wall` and — for a hook run — its
/// registered budget. Both bind the same foreground scope, tightest wins
/// (`min`, not `or`), so a host wall can never be silently widened by a
/// hook's own budget or vice versa. `None` from both leaves the run
/// unarmed, exactly as before.
fn arm_wall(
    wall: Option<std::time::Duration>,
    hook_budget: Option<std::time::Duration>,
    foreground: &crate::process::ForegroundScope,
) -> Option<crate::process::Deadline> {
    let effective = match (wall, hook_budget) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    effective.map(|d| crate::process::arm_lifetime(foreground.as_scope().clone(), d))
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
    /// Run one whole [`Run`] under `req`, synchronously, and return one
    /// flat [`RunReport`]. The single run door: the run's
    /// [`Program`] is resolved here — source text compiles and typechecks
    /// against the live session, a hook resolves in the session-lived hook
    /// table — and either program then runs through the shared framed
    /// scaffold ([`Self::run_built`]): materialising the IO regime, minting
    /// the run's foreground scope and arming its wall, installing the
    /// run-local surface, evaluating under the capability ceiling, and
    /// folding in the captured bytes and `timed_out`.
    ///
    /// Run completion is *this call returning* — never a channel
    /// disconnecting. A deferred worker may hold a clone of the surface sink
    /// forever; it changes nothing, because nothing waits on that sink to
    /// decide the run is over.
    ///
    /// The run door is also the durability boundary
    /// (`decisions/260706_enquiry-channel` §5): the shell checkpoints its
    /// [`Mobile`](crate::types::Mobile) at entry and a panic anywhere in the
    /// run — compile, eval, hook body, desk handler, ready-boundary
    /// housekeeping — restores it, so a panicked run reports as a failed
    /// run with the shell already rolled back. State-owner rollback, one
    /// mechanism for every transport and host; a snapshot never crosses the
    /// seam.
    pub fn run(&mut self, req: RunRequest<'_>) -> RunReport {
        let checkpoint = self.mobile.clone();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.dispatch(req))) {
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

    /// [`Self::run`]'s body, separated so the durability wrapper above
    /// is the only way through the door.
    fn dispatch(&mut self, mut req: RunRequest<'_>) -> RunReport {
        match req.run.program {
            Program::Source(ref src) => {
                // The two session ledgers' committed-run clock: one tick
                // per source dispatch, whether or not it goes on to compile —
                // a failed run ages the ledgers' scratch without renewing it
                // (`decisions/260629_agent-binding-reaping`). No-ops when
                // unarmed, so the REPL/batch pay two branches and nothing
                // else.
                self.local.bindings.tick();
                self.local.workers.tick_epoch();

                // Mint the run's foreground scope and arm its wall *before*
                // compiling, so the limit bounds the whole run — compile and
                // typecheck included, not only evaluation. `compile_run`'s
                // `process::clear` touches only the signal count, never the
                // reaper, so an entry armed here survives the compile. The
                // `Deadline` guard disarms when `wall` drops, so an early
                // `Static` return leaves no pending reaper entry.
                let foreground = self.durable_root().child();
                let wall = arm_wall(req.run.wall, None, &foreground);

                let (comp, single_command) = match compile_run(self, src) {
                    Ok(parts) => parts,
                    Err(diagnostics) => return RunReport::Static { diagnostics },
                };

                // "Committed" = reached evaluation: harvest the compiled
                // program's referenced names and renew every one that is
                // already leased. Gated on `armed()` so an unarmed host
                // (REPL, batch) never pays for the walk.
                if self.local.bindings.armed() {
                    self.local
                        .bindings
                        .renew(crate::ir::referenced_names(&comp));
                }

                self.run_built(req, &foreground, wall, single_command, |s| {
                    crate::evaluator::eval_top_level(&comp, s)
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

                // The host conveys data, not closures, across the dispatch
                // boundary: hook args are first-order by type (`FOValue`).
                let args: Vec<Value> = args.iter().cloned().map(Value::from).collect();

                let foreground = self.durable_root().child();
                let wall = arm_wall(req.run.wall, hook.policy.budget, &foreground);

                // Fold the hook's registered `DefaultPolicy` into the run's
                // conditions: capture, terminal authority, and budget are the
                // hook's to decide, not the dispatching host's.
                if hook.policy.capture {
                    req.run.io = RunIo::Capture;
                }
                req.run.terminal = match hook.policy.terminal {
                    TerminalPolicy::Denied => RequestedTerminalAccess::Denied,
                    TerminalPolicy::Leased => RequestedTerminalAccess::Leased,
                };

                self.run_built(req, &foreground, wall, false, |s| {
                    crate::builtins::apply(&hook.binding.value, &args, s)
                })
            }
        }
    }

    /// The framed scaffold behind the run door: materialise the IO regime
    /// from the run's conditions, build and install the run frame on the
    /// pre-minted `foreground`, evaluate `body` under the capability ceiling
    /// and lifecycle hooks, then disarm the `wall` and fold the captured
    /// bytes and `timed_out` into the report. `body` is the run's resolved
    /// program — the source arm's `eval_top_level`, the hook arm's in-frame
    /// `apply`.
    fn run_built(
        &mut self,
        req: RunRequest<'_>,
        foreground: &crate::process::ForegroundScope,
        wall: Option<crate::process::Deadline>,
        single_command: bool,
        body: impl FnOnce(&mut Self) -> Settled<Value>,
    ) -> RunReport {
        let RunRequest {
            run,
            surface,
            deferred,
            desk,
            nursery,
            lifecycle,
        } = req;

        // The source text the lifecycle hooks and root context see: the
        // program itself for a source run, empty for a hook run (whose
        // program is an already-compiled value, not text).
        let src = match &run.program {
            Program::Source(src) => src.as_str(),
            Program::Hook { .. } => "",
        };

        // Materialise the IO regime: `Capture` mints buffers we read back,
        // `Inherit` leaves the ambient streams to flow through `build_run`.
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

        // Stdin source and terminal authority are independent of the output
        // regime: `Capture` no longer implies `Source::Terminal`. A tool run
        // is `Denied` + `Empty`; a piped `ral -c` is `Denied` + `Inherit`.
        let stdin = match run.stdin {
            RunStdin::Inherit => Source::Terminal,
            RunStdin::Empty => Source::Empty,
        };
        let terminal_access = match run.terminal {
            RequestedTerminalAccess::Leased => crate::types::TerminalAccess::Leased,
            RequestedTerminalAccess::Denied => crate::types::TerminalAccess::Denied,
        };

        let next = build_run(
            self,
            capture,
            stdin,
            terminal_access,
            foreground.clone(),
            run.deferred_lease,
            run.worker_cap,
            surface,
            deferred,
            desk,
            nursery,
        );
        let (result, status) = run_framed(
            self,
            next,
            &run.script_name,
            src,
            run.caps.clone(),
            lifecycle,
            body,
        );

        // Disarm the wall before reading the cause. While it stays armed the
        // reaper can still fire; classifying against a live ceiling lets a run
        // that finished inside its budget be misread as timed out should the
        // reaper trip in the gap between eval returning and this read. Dropping
        // the guard removes the entry, so `cause` is `Deadline` below only for a
        // deadline that genuinely elapsed during the run.
        drop(wall);

        let timed_out = foreground.cause() == Some(CancelCause::Deadline);
        let captured = capture_bufs.map(|(out, err)| Captured {
            stdout: crate::io::take_buffer(&out),
            stderr: crate::io::take_buffer(&err),
        });

        RunReport::Ran {
            result,
            status,
            single_command,
            captured,
            timed_out,
        }
    }
}

// ── The spine behind the door: compile, build, install, classify ────────────

/// The transport status of one settled run: the success status for a
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

/// Saves the previous run frame on install and restores it on `Drop`,
/// and owns the per-run foreground and durable-root signal-slot
/// publications for the same extent. Restoring under `Drop` rather than an
/// explicit epilogue keeps the persistent `Shell` clean even when the
/// evaluation unwinds — a host that catches a worker panic and continues the
/// session on the same `Shell` would otherwise inherit a stale cursor, an
/// already-cancelled foreground scope, and (had the run captured IO) the
/// prior run's buffers and a foreign surface sink.
///
/// Only the signal-facing session publishes
/// (`SessionState::publishes_signal_slots`): the slots' save/restore
/// discipline is LIFO on one thread, which concurrent sessions — a host's
/// forked sub-agents dispatching runs on their own threads — would
/// violate; and a signal must target the primary session's run, never
/// whichever session dispatched last.  A non-publishing session's runs
/// are cancelled through its scope handles instead
/// ([`Shell::cancel_handle`](crate::types::Shell::cancel_handle)).
///
/// The slot publications point at raw flags inside the installed frame's
/// cancel scope, which [`publish_foreground`](crate::process::publish_foreground)
/// makes immortal. The guards therefore bound only *when* a slot fires: a slot
/// outliving the frame it names is stale, never dangling.
struct RunGuard<'s> {
    _fg: Option<CancelSlot>,
    _root: Option<CancelSlot>,
    shell: &'s mut Shell,
    saved: RunState,
}

impl<'s> RunGuard<'s> {
    /// Swap `next` into `shell.run`, publish the installed frame's
    /// foreground scope and the session's durable root into the
    /// signal-reachable slots (signal-facing session only), and hold the
    /// displaced frame for restoration on `Drop`.
    #[allow(
        clippy::used_underscore_binding,
        reason = "RAII guard field; the leading underscore marks it as held for Drop, not read"
    )]
    fn install(shell: &'s mut Shell, next: RunState) -> Self {
        let saved = std::mem::replace(&mut shell.run, next);
        let publish = shell.session.publishes_signal_slots;
        let _fg = publish.then(|| crate::process::publish_foreground(shell.run.cancel.as_scope()));
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

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        // Swap the saved frame back in; the displaced frame moves into `saved`
        // and drops with the guard.
        std::mem::swap(&mut self.shell.run, &mut self.saved);
        // Empty the displaced frame's nursery: a fork parked during the run
        // and never adopted (a refused enquiry, or a run that ended before
        // its handler ran) must not survive the run that parked it.
        if let Some(nursery) = self.saved.nursery.as_ref() {
            nursery.clear();
        }
    }
}

/// Build the [`RunState`] a run installs, seeded from the ambient `shell`.
///
/// `capture` is `Some` only under [`RunIo::Capture`],
/// where the run's byte *output* streams are redirected into host-read
/// buffers; under `Inherit` it is `None` and the ambient streams flow through
/// unchanged. `stdin` and `terminal_access` are supplied independently of the
/// output regime (the host's [`RunStdin`] and
/// [`RequestedTerminalAccess`]): stdin is
/// always installed, so `Capture` no longer implies `Source::Terminal`. The
/// `surface` is always the request's run-local sink — it is no longer carried
/// on the persistent session, so it has no liveness role.  `deferred` is the
/// session-lived destination a deferred worker delivers its surface batch to
/// when it settles.  `desk` is the request's run-local enquiry desk; `None`
/// outside a host that answers enquiries.  `nursery` is the request's
/// run-local nursery for engine-side session forks; `None` outside a host
/// that installs one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_run(
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
    nursery: Option<Nursery>,
) -> RunState {
    let mut run_io = shell.run.io.try_clone().unwrap_or_else(|_| Io {
        terminal: shell.run.io.terminal,
        interactive: shell.run.io.interactive,
        launch_role: shell.run.io.launch_role,
        ..Io::default()
    });
    run_io.stdin = stdin;
    if let Some((stdout, stderr)) = capture {
        run_io.stdout = stdout;
        run_io.stderr = stderr;
    }
    RunState {
        io: run_io,
        surface,
        deferred,
        desk,
        nursery,
        cancel: foreground,
        loc: LocationCursor::default(),
        deferred_lease,
        worker_cap,
        terminal_access,
    }
}

/// Clear signal state, compile and typecheck `src` against the live session.
/// `Ok((comp, single_command))` on a clean compile; `Err(diagnostics)` on a
/// parse or type failure (the run never reaches evaluation). The
/// `single_command` flag is harvested here because the host needs it to render
/// runtime errors after `comp` is consumed.
///
/// Resets the session's source registry and peeks the [`FileId`] the next
/// registration will mint *before* compiling, so the compiled program's
/// spans carry this run's real file identity. [`Shell::install_root_context`]
/// performs the actual reset-then-register once the run frame exists to
/// install it into ([`run_framed`]); nothing between here and there touches
/// the registry, so the id it mints always agrees with the one peeked here.
pub(crate) fn compile_run(
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

/// Install the built run state, fire the lifecycle hooks around the eval
/// `body` under `capabilities`, compute the transport status, and tear the
/// frame down before returning `(result, status)`. The `body` is the run's
/// program: the source door evaluates a compiled [`Comp`](crate::ir::Comp)
/// (`eval_top_level`); the value door applies an already-evaluated thunk in
/// place (`builtins::apply`). Both settle to one [`Settled<Value>`], so the
/// lifecycle/capability/status spine is shared verbatim. The `RunGuard`
/// restores the prior frame on `Drop`, even on unwind. The caller
/// ([`Shell::run_built`]) owns IO materialisation, limit arming, and the
/// `timed_out`/capture classification.
pub(crate) fn run_framed<'a>(
    shell: &mut Shell,
    next: RunState,
    script_name: &str,
    src: &str,
    capabilities: Capabilities,
    mut lifecycle: Box<dyn RunLifecycle + 'a>,
    body: impl FnOnce(&mut Shell) -> Settled<Value>,
) -> (Settled<Value>, i32) {
    let mut guard = RunGuard::install(shell, next);
    let shell = guard.shell_mut();
    shell.install_root_context(script_name.to_string(), src);

    lifecycle.pre_exec(shell, src);

    let result = shell.with_capabilities(capabilities, body);

    let status = eval_status(&result, shell);

    lifecycle.post_exec(shell, src, status);

    // Push this run's ready-boundary housekeeping — the lease chain's reap
    // notices as `` `notice `` surface classes, the large-binding warning onto
    // the run's own stderr — while this run's frame (its surface sink and
    // capture streams) is still installed, so each rides this run's own
    // stream, ordered before its Report (`decisions/260706_enquiry-channel`
    // §4.2). After `guard` drops below, `shell.run` reverts and there is no
    // sink left to push through.
    shell.emit_ready_boundary_notices();

    drop(guard);

    (result, status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::ByteBuffer;
    use std::sync::{Arc, Mutex};

    /// A capturing request under the ⊤ capability ceiling with no surface
    /// sink and no lifecycle hooks — the minimal request a host with no
    /// surface decoder or plugin policy supplies. Mirrors exarch's tool run:
    /// foreground `Denied`, stdin `Empty`. Core mints the capture buffers and
    /// returns them in `RunReport::Ran { captured, .. }`.
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

    /// A clean run over a simple expression settles to an `Ok` value
    /// with a zero transport status.
    #[test]
    fn clean_run_settles_with_zero_status() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        match shell.run(capture_req("$[1 + 1]")) {
            RunReport::Ran { result, status, .. } => {
                assert_eq!(status, 0);
                assert!(result.is_ok(), "expected Ok, got {result:?}");
            }
            RunReport::Static { .. } => panic!("clean source must not be Static"),
        }
    }

    /// Malformed source never reaches evaluation: it returns a parse
    /// diagnostic.
    #[test]
    fn parse_failure_is_static() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
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

    /// A type error never reaches evaluation: it returns type diagnostics.
    #[test]
    fn type_failure_is_static() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
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

    /// `exit N` settles to an `Escape::Exit(N)` and reports N as the
    /// transport status.
    #[test]
    fn exit_escape_reports_code() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
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

    /// `RunIo::Capture` returns the run's stdout in `Ran::captured`.
    #[test]
    fn capture_returns_stdout_bytes() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
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

    /// The run's foreground scope and run-local surface are torn down
    /// before `run` returns: the surface is restored to its pre-run
    /// value, and cancelling the restored `shell.run.cancel` reaches the
    /// pre-run scope (proving the guard swapped the pre-run frame back in,
    /// not the run's internally-minted child).
    #[test]
    fn frame_restores_state_on_return() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        assert!(
            shell.run.surface.is_none(),
            "no surface sink before the run"
        );
        assert!(!shell.run.cancel.is_cancelled(), "pre-run scope is live");
        let pre = shell.foreground().clone();

        let _ = shell.run(RunRequest {
            surface: Some(Arc::new(())),
            ..capture_req("$[1 + 1]")
        });

        assert!(
            shell.run.surface.is_none(),
            "run-local surface must be restored to its pre-run value"
        );
        // Cancelling the restored foreground must reach the pre-run scope:
        // the teardown swapped the pre-run frame back in, discarding the
        // run's internally-minted child.
        shell
            .foreground()
            .cancel(crate::process::CancelCause::Explicit);
        assert!(
            pre.is_cancelled(),
            "foreground scope must be restored to the pre-run scope"
        );
    }

    /// A no-op desk, just enough to install a non-`None` value on the run.
    struct NoopDesk;
    impl crate::types::EnquiryDesk for NoopDesk {
        fn enquire(
            &self,
            req: crate::serial::FOValue,
        ) -> Result<crate::serial::FOValue, crate::types::Error> {
            Ok(req)
        }
    }

    /// The desk twin of `frame_restores_state_on_return`: the run-local
    /// enquiry desk is torn down before `run` returns, restored
    /// to its pre-run value (`None`) exactly as `surface` is.
    #[test]
    fn desk_is_restored_to_its_pre_run_value() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        assert!(shell.run.desk.is_none(), "no desk before the run");

        let _ = shell.run(RunRequest {
            desk: Some(Arc::new(NoopDesk)),
            ..capture_req("$[1 + 1]")
        });

        assert!(
            shell.run.desk.is_none(),
            "run-local desk must be restored to its pre-run value"
        );
    }

    /// A `RunLifecycle` that parks a fork into the run's nursery during
    /// `pre_exec` (mirroring how a desk handler's builtin body would) and
    /// records the minted id for the test to redeem afterward.
    struct ParkDuringRun(std::sync::Arc<Mutex<Option<crate::types::NurseryId>>>);
    impl RunLifecycle for ParkDuringRun {
        fn pre_exec(&mut self, shell: &mut Shell, _src: &str) {
            let id = shell
                .fork_into_nursery()
                .expect("a nursery is installed on this run");
            *self.0.lock().unwrap() = Some(id);
        }
    }

    /// The nursery twin of `desk_is_restored_to_its_pre_run_value`: the
    /// run-local nursery is torn down before `run` returns, restored to
    /// its pre-run value (`None`) exactly as `desk` is — and, beyond mere
    /// restoration, a fork parked during the run and never adopted is gone
    /// from the host's own `Nursery` clone afterward, proving the run guard
    /// empties it rather than merely swapping it out.
    #[test]
    fn nursery_is_restored_and_emptied_at_run_teardown() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        assert!(shell.run.nursery.is_none(), "no nursery before the run");

        let nursery = crate::types::Nursery::default();
        let parked_id: Arc<Mutex<Option<crate::types::NurseryId>>> = Arc::new(Mutex::new(None));

        let _ = shell.run(RunRequest {
            nursery: Some(nursery.clone()),
            lifecycle: Box::new(ParkDuringRun(parked_id.clone())),
            ..capture_req("$[1 + 1]")
        });

        assert!(
            shell.run.nursery.is_none(),
            "run-local nursery must be restored to its pre-run value"
        );
        let id = parked_id
            .lock()
            .unwrap()
            .expect("pre_exec must fork into the nursery");
        assert!(
            nursery.adopt(id).is_none(),
            "a fork parked during the run and never adopted must not survive the run's teardown"
        );
    }

    /// A settling run's ready-boundary housekeeping surfaces the expected
    /// `` `notice `` class (`decisions/260706_enquiry-channel` §4.2): a
    /// worker the lease chain reaps between runs has no live sink to push
    /// through until the *next* run installs one — installed here on that
    /// next run, it captures the reap as a `notice` variant, ordered before
    /// this run's own settling.
    #[test]
    fn ready_boundary_notice_surfaces_a_pending_worker_reap() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());

        // Spawn a worker under a millisecond-scale idle lease and never
        // poll it, so the background lease chain reaps it quickly.
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

        // A live sink installed only on this *next* run: the pending reap
        // has nowhere to push through until now.
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

    /// A lifecycle test double records that both hooks fired and that
    /// `post_exec` saw the computed transport status.
    #[test]
    fn lifecycle_hooks_fire_with_status() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
        #[derive(Clone)]
        struct Spy {
            pre: Arc<Mutex<bool>>,
            post_status: Arc<Mutex<Option<i32>>>,
        }
        impl RunLifecycle for Spy {
            fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {
                *self.pre.lock().unwrap() = true;
            }
            fn post_exec(&mut self, _shell: &mut Shell, _src: &str, status: i32) {
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

    /// `run` publishes the installed foreground scope into the
    /// signal-reachable slot for the eval's extent: a lifecycle hook that
    /// requests a foreground cancel (the role a signal handler plays) reaches
    /// `shell.run.cancel`, and the trampoline's `process::check` unwinds
    /// the eval into a `Break::Error` with transport status 130.
    #[test]
    fn published_foreground_slot_carries_request_into_eval() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
        struct CancelInPreExec;
        impl RunLifecycle for CancelInPreExec {
            fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Interrupt);
            }
        }

        let mut shell = Shell::new(crate::io::TerminalState::default());
        // Publication under test: opt in (test sessions default out).
        shell.session.publishes_signal_slots = true;
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

    /// A forked session ([`Shell::fork_session`]) is not the signal-facing
    /// session, so its run doors leave the signal slots untouched: a
    /// foreground-cancel request fired mid-run (the role a signal handler
    /// plays) finds no published slot and the eval completes undisturbed.
    /// Its host stops it through [`Shell::cancel_handle`] instead — the
    /// companion assertion below cancels the handle and sees the same
    /// unwind a published slot would have produced.
    #[test]
    fn forked_session_run_does_not_publish_signal_slots() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
        struct CancelInPreExec;
        impl RunLifecycle for CancelInPreExec {
            fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Interrupt);
            }
        }

        let mut trunk = Shell::new(crate::io::TerminalState::default());
        // Mark the trunk signal-facing so the fork's non-publication below
        // is `fork_session`'s doing, not the test default's.
        trunk.session.publishes_signal_slots = true;
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

        // The host-side path: cancelling the forked session's handle unwinds
        // its next run at the evaluator's poll, exactly as a published slot
        // would have for the signal-facing session.
        struct CancelHandleInPreExec(crate::process::DurableRoot);
        impl RunLifecycle for CancelHandleInPreExec {
            fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {
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

    /// `run` publishes the durable root into the signal-reachable
    /// root slot for the run's extent: a lifecycle hook that requests a
    /// root abort (the role a SIGQUIT handler plays) reaches the root, and
    /// the foreground scope — a child of the root — observes the abort
    /// through its parent chain. The trampoline's `process::check` unwinds
    /// the eval into a `Break::Error` with transport status 130.
    #[test]
    fn published_root_slot_carries_abort_into_eval() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
        struct AbortInPreExec;
        impl RunLifecycle for AbortInPreExec {
            fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {
                crate::process::request_root_cancel(crate::process::CancelCause::RootAbort);
            }
        }

        let mut shell = Shell::new(crate::io::TerminalState::default());
        // Publication under test: opt in (test sessions default out).
        shell.session.publishes_signal_slots = true;
        match shell.run(RunRequest {
            lifecycle: Box::new(AbortInPreExec),
            ..capture_req("let x = 42\nreturn $x")
        }) {
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

    /// A run whose wall is armed and whose foreground scope is
    /// cancelled with `Deadline` reports `timed_out`. Driven through a
    /// lifecycle hook that fires the deadline cause directly, so the test
    /// pins the `timed_out` classification without sleeping on the reaper.
    #[test]
    fn deadline_cancel_reports_timed_out() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
        struct DeadlineInPreExec;
        impl RunLifecycle for DeadlineInPreExec {
            fn pre_exec(&mut self, _shell: &mut Shell, _src: &str) {
                crate::process::request_foreground_cancel(crate::process::CancelCause::Deadline);
            }
        }

        let mut shell = Shell::new(crate::io::TerminalState::default());
        // Publication under test: opt in (test sessions default out).
        shell.session.publishes_signal_slots = true;
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

    /// Under `RunIo::Inherit` the run evaluates on a clone of the session's
    /// streams and the guard restores them on teardown: the session's stdout
    /// sink is the *same* object before and after the run (its `Arc` is
    /// shared into the run and restored after), and the run's output lands
    /// in it.
    #[test]
    fn inherit_leaves_session_streams_untouched() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let marker: ByteBuffer = Arc::new(Mutex::new(Vec::new()));
        shell.run.io.stdout = Sink::Buffer(marker.clone());

        let req = capture_req("echo hi");
        let _ = shell.run(RunRequest {
            run: Run {
                io: RunIo::Inherit,
                ..req.run
            },
            ..req
        });

        assert!(
            matches!(&shell.run.io.stdout, Sink::Buffer(b) if Arc::ptr_eq(b, &marker)),
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
    // The run door is the durability boundary (`decisions/260706_
    // enquiry-channel` §5): a panic anywhere in the run reports as a
    // failed run with the shell's `Mobile` already rolled back to its
    // run-entry state. State-owner rollback — no host holds a snapshot.

    /// A nullary builtin whose body panics — stands in for any Rust panic
    /// the evaluator can raise mid-run.
    fn builtin_panic_now(
        _args: &[crate::types::Value],
        _shell: &mut Shell,
    ) -> crate::types::Settled<crate::types::Value> {
        panic!("run-door test: deliberate mid-eval panic");
    }

    fn scheme_panic_now(_u: &mut crate::typecheck::Unifier) -> crate::typecheck::Scheme {
        use crate::typecheck::builtins::{mk_scheme, pure, thunk};
        mk_scheme(&[], &[], &[], thunk(pure(crate::typecheck::Ty::Unit)))
    }

    static PANIC_BUILTINS: &[crate::types::BuiltinEntry] = &[crate::types::BuiltinEntry {
        name: std::borrow::Cow::Borrowed("core-panic-now"),
        type_rule: crate::typecheck::builtins::BuiltinTypeRule::Scheme(Some(0), scheme_panic_now),
        doc: "test-only: panic the evaluator mid-run.",
        body: crate::types::BuiltinBody::Static(builtin_panic_now),
    }];

    /// A mid-eval panic reports as a failed run naming the panic, with the
    /// panicking run's own partial mutations rolled back, a pre-run
    /// binding intact, and the shell evaluating the next run cleanly.
    #[test]
    fn panicking_run_reports_failed_and_rolls_back() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
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

    /// A panic in a lifecycle hook — after evaluation, outside the
    /// evaluator proper — is caught by the same door: the whole run spine
    /// is inside the checkpoint.
    struct PanicInPostExec;
    impl RunLifecycle for PanicInPostExec {
        fn post_exec(&mut self, _shell: &mut Shell, _src: &str, _status: i32) {
            panic!("run-door test: post_exec panic");
        }
    }

    #[test]
    fn lifecycle_panic_is_caught_at_the_door() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
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

    /// A desk handler that panics mid-enquiry unwinds through the run and
    /// is caught at the door like any other panic during a run.
    struct PanickingDesk;
    impl crate::types::EnquiryDesk for PanickingDesk {
        fn enquire(
            &self,
            _req: crate::serial::FOValue,
        ) -> Result<crate::serial::FOValue, crate::types::Error> {
            panic!("run-door test: desk handler panic");
        }
    }

    struct EnquireInPreExec;
    impl RunLifecycle for EnquireInPreExec {
        fn pre_exec(&mut self, shell: &mut Shell, _src: &str) {
            let _ = shell.enquire(crate::serial::FOValue::Unit);
        }
    }

    #[test]
    fn desk_handler_panic_is_caught_at_the_door() {
        let _slot_guard = crate::process::cancel::SLOT_SERIAL.lock();
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
}
