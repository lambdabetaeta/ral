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

use crate::capability::GrantStack;
use crate::compile::compile_and_typecheck;
use crate::diagnostic::{Rejection, Report};
use crate::io::{Captured, Io, RunIo, RunStdin, Sink, Source};
use crate::process::{CancelCause, ForegroundScope, RequestedTerminalAccess};
use crate::protocol::{Program, Run};
use crate::source::{FileId, Span};
use crate::types::{
    Break, DeferredSink, Desk, Error, Escape, Fork, Mooring, Notice, NurseryGuard, Observation,
    Settled, Shell, SurfaceSink, TrailScope, Value,
};
use std::io::Write as _;
use std::sync::Arc;

mod report;

/// Parse/type diagnostics from a run that never reached evaluation.
///
/// Not folded into [`Error`]: a loaded file's parse error would then raise
/// status 2, not 1.
pub enum StaticDiagnostics {
    Compile(Rejection),
    /// A host-level error that stopped the run before it started: hook not
    /// found, non-ground argument, and the like. Spanless, so no text.
    Host(Error),
}

impl StaticDiagnostics {
    /// The run drawn whole, with the exit status it ends on.
    pub fn render(&self) -> (String, i32) {
        match self {
            Self::Compile(rejection) => (rejection.render(), rejection.status),
            Self::Host(e) => {
                let report = Report {
                    code: None,
                    message: e.message.clone(),
                    at: None,
                    also: None,
                    hint: e.hint.clone(),
                };
                (report.plain(), e.code())
            }
        }
    }
}

// ── The run entry: one synchronous, runtime-agnostic host door ──────────────
//
// Hosts describe *policy*; core owns *resources*. The reduction primitive
// behind the door is crate-private, so no host can start an unframed
// evaluation that would foreground or capture against a stale frame.

/// The engine door for one run: the protocol [`Run`] plus the live,
/// non-transportable handles the host lends it.
///
/// Composition, not mirroring — a field added to [`Run`] reaches the engine
/// in one declaration.
pub struct RunRequest {
    pub run: Run,
    /// Run-local sink for structured events; `None` is the identity.
    pub surface: Option<SurfaceSink>,
    /// Session-lived destination a settling worker delivers its surface batch
    /// to; `None` leaves it reachable only through `await`/`race`.
    pub deferred: Option<Arc<dyn DeferredSink>>,
    /// Run-local enquiry desk. Same-thread children inherit it, as they do the
    /// fork door below; deferred workers get neither.
    pub desk: Option<Desk>,
    /// How a forked session reaches this run's desk; `None` adopts none.
    pub fork: Option<Fork>,
}

impl From<Run> for RunRequest {
    /// A request lending the host's run nothing: no surface, deferred sink,
    /// desk or fork door.
    fn from(run: Run) -> Self {
        Self {
            run,
            surface: None,
            deferred: None,
            desk: None,
            fork: None,
        }
    }
}

/// One report the host matches once — a `Static` run never ran.
pub enum RunReport {
    /// The run never reached evaluation; the host renders the diagnostics and
    /// treats it as status 1.
    Static { diagnostics: StaticDiagnostics },
    Ran {
        ending: Ending,
        captured: Option<Captured>,
        /// This dispatch's own trail, filled in by [`Shell::enter`] once the
        /// scope it held closes; empty when `Run.trail` was `None`.
        trail: Vec<Observation>,
    },
}

/// How a run left evaluation — one arm per way out.
///
/// `Ok` beside a timed-out flag is not a state the type can hold.
/// Unrendered: the error text is rendered once, in
/// [`Report`](crate::protocol::Report), where a
/// [`SourceDb`](crate::source::SourceDb) is in hand.
#[derive(Debug)]
pub enum Ending {
    Settled {
        value: Value,
        status: i32,
    },
    /// `compact` is the root source of a one-command program, the one that
    /// picks the compact runtime-error rendering.
    Raised {
        error: Error,
        compact: Option<FileId>,
    },
    /// A raise the wall itself caused — same shape as [`Self::Raised`], the
    /// tag alone is what a renderer needs to choose the timeout remedy over
    /// the exit-code one.
    Walled {
        error: Error,
        compact: Option<FileId>,
    },
    Exited(i32),
}

impl Ending {
    /// The transport status: ambient for [`Self::Settled`] (captured at
    /// construction, since it reads session state a later run could change),
    /// a pure function of the escape for every other arm.
    pub fn status(&self) -> i32 {
        match self {
            Self::Settled { status, .. } => *status,
            Self::Raised { error, .. } | Self::Walled { error, .. } => error.code(),
            Self::Exited(code) => *code,
        }
    }

    /// Recover the flat view a caller that only wants pass/fail still wants.
    ///
    /// # Errors
    /// Every non-[`Self::Settled`] arm, folded back into the [`Break`] it
    /// came from.
    pub fn into_result(self) -> Settled<Value> {
        match self {
            Self::Settled { value, .. } => Ok(value),
            Self::Raised { error, .. } | Self::Walled { error, .. } => Err(Break::Error(error)),
            Self::Exited(code) => Err(Break::Escape(Escape::Exit(code))),
        }
    }
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
    /// run restores the `env`/`context` checkpointed at entry, so the shell
    /// rolls itself back and no snapshot crosses the engine protocol.
    pub fn run(&mut self, req: impl Into<RunRequest>) -> RunReport {
        let anchor = self.session.anchor.clone();
        self.run_under(&anchor, req.into())
    }

    /// Run `req` with its frame under a scope the host minted with
    /// [`Shell::run_cancel_handle`] rather than under the session anchor — the
    /// door a host takes when it must be able to cancel this run from another
    /// thread, including before the run has begun.  [`Shell::run`] is this door
    /// with the anchor.
    ///
    /// A dispatch that asks for a trail (`req.run.trail: Some`) gets its
    /// scope held *here*, outside the `catch_unwind` below: the checkpoint
    /// `dispatch`'s panic arm rolls back does not cover `local.audit`
    /// (`LocalState` partitions it deliberately), so only a scope the panic
    /// cannot skip keeps the opener's close law true at dispatch
    /// granularity. A panic reports `Static` by declaration — the trail
    /// closes and is discarded, never attached.
    pub(crate) fn run_under(&mut self, under: &ForegroundScope, req: RunRequest) -> RunReport {
        // A run is the extent of ral's picture of `PATH`: whatever happened to
        // the filesystem between runs is admitted here, and within a run a
        // walk is paid once per name.
        crate::path::forget_located_commands();
        let checkpoint = (self.env.clone(), self.context.clone());
        let scope: Option<TrailScope> = req.run.trail.map(|policy| self.local.audit.open(policy));

        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.dispatch(under, req)));

        let trail = scope.map(|scope| self.local.audit.close(scope));

        match outcome {
            Ok(mut report) => {
                if let (Some(trail), RunReport::Ran { trail: field, .. }) = (trail, &mut report) {
                    *field = trail;
                }
                report
            }
            Err(payload) => {
                (self.env, self.context) = checkpoint;
                RunReport::Static {
                    diagnostics: StaticDiagnostics::Host(crate::types::Error::raised(
                        format!("run panicked: {}", panic_text(payload.as_ref())),
                        101,
                    )),
                }
            }
        }
    }

    /// Resolve the run's [`Program`] and hand it to the framed scaffold, with
    /// this run's foreground frame minted under `under`.
    fn dispatch(&mut self, under: &ForegroundScope, mut req: RunRequest) -> RunReport {
        // Armed *before* compiling, so the limit bounds compile and typecheck
        // too: `compile_run`'s `process::clear` touches only the signal
        // escalation count, never the reaper. The guard disarms on drop, so
        // an early `Static` return leaves no entry.
        // Nested under the frame it displaces, so the tree *is* the runs'
        // dynamic extent: an outer run's cancel or wall reaches into the nest.
        let foreground = under.child();
        let wall = req.run.wall.map(|d| arm_wall(d, &foreground));

        match req.run.program {
            Program::Source(ref src) => {
                // The two session ledgers' clock: one tick per source dispatch
                // whether or not it goes on to compile, so a failed run ages
                // their scratch without renewing it. Both no-op when unarmed.
                self.local.bindings.tick();
                self.local.workers.tick_epoch();

                let (top, compact) = match compile_run(self, src, &req.run.script_name) {
                    Ok(parts) => parts,
                    Err(diagnostics) => {
                        return RunReport::Static {
                            diagnostics: *diagnostics,
                        };
                    }
                };

                // Reaching evaluation is what renews a lease. Gated so an
                // unarmed host skips the walk over referenced names.
                if self.local.bindings.armed() {
                    self.local.bindings.renew(top.referenced_names());
                }

                self.run_built(req, foreground, wall, compact, |m, s| {
                    crate::evaluator::readmit(&top, s)?;
                    crate::evaluator::run_phrases(
                        &top.phrases,
                        s.env.clone(),
                        crate::evaluator::Mode::Session,
                        m,
                        s,
                    )
                    .outcome
                })
            }
            Program::Hook { ref name, ref args } => {
                let Some(hook) = self.session.hooks.get(name).cloned() else {
                    return RunReport::Static {
                        diagnostics: StaticDiagnostics::Host(crate::types::Error::new(format!(
                            "hook '{name}' is not registered"
                        ))),
                    };
                };

                // The host conveys data, not closures: hook args are
                // first-order by type (`FOValue`).
                let args: Vec<Value> = args.iter().cloned().map(Value::from).collect();

                // Capture, terminal authority and aside are the registered
                // hook's to decide, not the dispatching host's.
                if hook.policy.io == RunIo::Capture {
                    req.run.io = RunIo::Capture;
                }
                req.run.terminal = hook.policy.terminal;

                let label = name.fault_label();
                let body = move |m: &Mooring, s: &mut Self| {
                    crate::evaluator::apply(&hook.binding.value, args, m, s).map_err(
                        |brk| match brk {
                            Break::Error(e) => Break::Error(e.context(label)),
                            escape @ Break::Escape(_) => escape,
                        },
                    )
                };
                if hook.policy.aside {
                    let mut aside = self.join_session();
                    aside.io = build_run(self, None, Source::Empty);
                    return aside.run_built(req, foreground, wall, None, body);
                }
                self.run_built(req, foreground, wall, None, body)
            }
        }
    }

    /// The framed scaffold both program arms share, `body` being the resolved
    /// program — `run_phrases` for source, `evaluator::apply` for a hook.
    ///
    /// The [`Mooring`] is an owned local on *this* stack frame and is only ever
    /// lent onward, so an outer run's is restored by the unwinding rather than
    /// by a guard; the [`NurseryGuard`] beside it empties its nursery on the
    /// panic path too, both being locals inside `enter`'s `catch_unwind`.
    fn run_built(
        &mut self,
        req: RunRequest,
        foreground: ForegroundScope,
        wall: Option<crate::process::Deadline>,
        compact: Option<FileId>,
        body: impl FnOnce(&Mooring, &mut Self) -> Settled<Value>,
    ) -> RunReport {
        let RunRequest {
            run,
            surface,
            deferred,
            desk,
            fork,
        } = req;

        // A hook run has no text: its program is an already-compiled value.
        let src = match &run.program {
            Program::Source(src) => Some(src.as_str()),
            Program::Hook { .. } => None,
        };

        let capture = (run.io == RunIo::Capture).then(Capture::default);

        let stdin = match run.stdin {
            RunStdin::Inherit => Source::Terminal,
            RunStdin::Empty => Source::Empty,
        };
        let terminal_access = match run.terminal {
            RequestedTerminalAccess::Leased => crate::types::TerminalAccess::Leased,
            RequestedTerminalAccess::Denied => crate::types::TerminalAccess::Denied,
        };

        let _nursery_guard = NurseryGuard(fork.clone());
        let mooring = Mooring {
            surface,
            deferred,
            desk,
            fork,
            cancel: foreground,
            deferred_lease: run.deferred_lease,
            worker_cap: run.worker_cap,
            terminal_access,
        };
        let next = build_run(self, capture.as_ref(), stdin);
        let (result, status) = run_framed(
            &mooring,
            self,
            next,
            &run.script_name,
            src,
            run.caps.clone(),
            body,
        );

        // Disarm before reading the cause: while armed, the reaper can still
        // trip in the gap between eval returning and this read, and a run that
        // finished inside its budget would be misread as timed out.
        drop(wall);

        let timed_out = mooring.cancel.cause() == Some(CancelCause::TimedOut);

        RunReport::Ran {
            ending: classify_ending(result, status, compact, timed_out),
            captured: capture.map(Capture::finish),
            // Filled in by `enter`, which holds this dispatch's scope.
            trail: Vec::new(),
        }
    }
}

/// Fold a settled body's raw parts into the one [`Ending`] they describe.
/// `status` is used only for [`Ending::Settled`] — every other arm's status
/// is a pure function of the escape, read lazily through [`Ending::status`].
fn classify_ending(
    result: Settled<Value>,
    status: i32,
    compact: Option<FileId>,
    timed_out: bool,
) -> Ending {
    match result {
        Ok(value) => Ending::Settled { value, status },
        // The cancel is stamped on the innermost node it unwound through, so
        // the wall is no exception to the engine's own error rendering —
        // only the tag distinguishing it from an ordinary raise is new here.
        Err(Break::Error(error)) if timed_out => Ending::Walled { error, compact },
        Err(Break::Error(error)) => Ending::Raised { error, compact },
        Err(Break::Escape(Escape::Exit(code))) => Ending::Exited(code),
    }
}

// ── The spine behind the door: compile, build, install, classify ────────────

/// Holds what a run takes on loan from the shell — the byte streams,
/// `session.root_file`, `local.audit.call_site` — and restores it on `Drop`, so
/// an unwinding run leaves nothing stale on the persistent `Shell`. Everything
/// else a run installs lives on the [`Mooring`], which was never on the shell
/// and so needs no restoring.
struct IoLoan<'s> {
    shell: &'s mut Shell,
    saved: Io,
    saved_root: Option<FileId>,
    saved_site: Option<Span>,
}

impl<'s> IoLoan<'s> {
    /// The run starts with the registers cleared, not inherited: a fresh run
    /// has no call site and no root file until it registers one.
    fn install(shell: &'s mut Shell, next: Io) -> Self {
        let saved = std::mem::replace(&mut shell.io, next);
        let saved_root = shell.session.root_file.take();
        let saved_site = shell.local.audit.call_site.take();
        Self {
            shell,
            saved,
            saved_root,
            saved_site,
        }
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
pub(crate) fn build_run(shell: &Shell, capture: Option<&Capture>, stdin: Source) -> Io {
    let (stdout, stderr) = capture.map_or_else(
        || (shell.io.stdout.clone(), shell.io.stderr.clone()),
        Capture::sinks,
    );
    Io {
        stdin,
        stdout,
        stderr,
        interactive: shell.io.interactive,
        terminal: shell.io.terminal,
        stage: shell.io.stage.clone(),
    }
}

/// The buffers a [`RunIo::Capture`] run's sinks fill.
#[derive(Default)]
pub(crate) struct Capture {
    stdout: crate::io::ByteBuffer,
    stderr: crate::io::ByteBuffer,
}

impl Capture {
    fn sinks(&self) -> (Sink, Sink) {
        (
            Sink::Buffer(self.stdout.clone()),
            Sink::Buffer(self.stderr.clone()),
        )
    }

    fn finish(self) -> Captured {
        Captured {
            stdout: self.stdout.take(),
            stderr: self.stderr.take(),
        }
    }
}

/// Clear signal state, then compile and typecheck `src` against the live
/// session.
///
/// The root [`FileId`], when the program is one command, comes back here
/// because the host needs it to render runtime errors once `comp` is
/// consumed, and it cannot be read back later: [`IoLoan`] restores
/// `session.root_file` on drop. The id is *peeked* before compiling so the
/// program's spans carry this run's file identity, while
/// [`Shell::install_root_context`] registers for real from [`run_framed`], once
/// a frame exists to install into — sound only because nothing in between
/// registers a source and the registry never shrinks.
pub(crate) fn compile_run(
    shell: &Shell,
    src: &str,
    name: &str,
) -> Result<(Arc<crate::ir::Toplevel>, Option<FileId>), Box<StaticDiagnostics>> {
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
    let outcome = compile_and_typecheck(src, schemes, file, name, None);
    crate::dbg_trace!(
        "shell",
        "compile_and_typecheck: {n_bindings} bindings, {} src bytes in {:?}",
        src.len(),
        t_tc.elapsed()
    );
    // The text is copied only here, on the failure path, and dies with the
    // report: `file` was peeked, never minted, so the registry is untouched.
    let top = Arc::new(outcome.map_err(|error| {
        Box::new(StaticDiagnostics::Compile(
            error.reject(crate::source::Source::from_text(name, src)),
        ))
    })?);

    let compact = top.is_single_command().then_some(file);
    Ok((top, compact))
}

/// Install the built run state, run `body` under `capabilities`, compute the transport status, and tear the frame down. Both
/// program arms settle to one [`Settled<Value>`], so this spine is shared
/// verbatim; the caller keeps the mooring, the limits and the classification.
pub(crate) fn run_framed(
    mooring: &Mooring,
    shell: &mut Shell,
    next: Io,
    script_name: &str,
    src: Option<&str>,
    capabilities: GrantStack,
    body: impl FnOnce(&Mooring, &mut Shell) -> Settled<Value>,
) -> (Settled<Value>, i32) {
    let guard = IoLoan::install(shell, next);
    let shell = &mut *guard.shell;
    // Only a source run registers: a hook's spans point into the file the hook
    // was defined in, so an entry for its absent text would name nothing and
    // never be reclaimed — the registry is append-only for the session's life.
    if let Some(src) = src {
        shell.install_root_context(script_name, src);
    }
    let boundary = src.is_some();

    let result = shell.with_layers(capabilities, |s| body(mooring, s));

    // Cancellation is sticky until the run settles, and the run settles here: a
    // recovery construct (`try`) classifies the cancellation `Break::Error`
    // like any other and may absorb it into a clean value, but the scope is
    // monotone, so this final poll re-raises it before the status is read —
    // the transport sees the true verdict. Demotes only a
    // successful settle; an error or escape already carries its own.
    let result = result.and_then(|value| mooring.check().map(|()| value));

    let status = result.as_ref().err().map_or(0, Break::code);

    // Ready-boundary housekeeping — reap notices as `` `notice `` surface
    // classes, the large-binding warning onto stderr — must go out while this
    // run's frame is still installed, so each rides this run's own stream and
    // is ordered before its Report. Once `guard` drops there is no sink left.
    // A hook run is no ready boundary.
    if boundary {
        shell.emit_ready_boundary_notices(mooring);
    }

    drop(guard);

    (result, status)
}

impl Shell {
    /// This run's ready-boundary housekeeping: [`run_framed`] calls it once per
    /// settled source run, *before* the frame tears down, so it rides the
    /// run's own streams and lands ahead of its report.
    ///
    /// The large-binding warning goes to stderr, reaching the model in its tool
    /// result rather than becoming a frontend card, and is ungated: stderr is
    /// there whether a surface sink is or not.  Reap and prune go out as
    /// [`Notice`]s; absent a sink that half leaves the ledgers *untouched*
    /// rather than drained and dropped, so their notices wait for a run that
    /// does install one.
    fn emit_ready_boundary_notices(&mut self, mooring: &Mooring) {
        // Above the sink guard: expiry is a fact regardless of anyone
        // listening, and its notice waits in the ledger either way.
        self.local.workers.sweep_retention();
        for notice in self.local.bindings.take_large_binding_notices() {
            let _ = writeln!(self.io.stderr, "{notice}");
        }
        if mooring.surface.is_none() {
            return;
        }
        for reap in self.take_worker_reap_notices() {
            mooring.surface_data(&Notice::Reap(reap).to_surface());
        }
        let pruned = self.local.bindings.prune(&mut self.env);
        if !pruned.is_empty() {
            mooring.surface_data(&Notice::Prune(pruned).to_surface());
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test scaffolding")]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Install `name`, a builtin running `act` inside whichever run calls it,
    /// with that run's mooring — how a test acts mid-run, as an engine-side
    /// door does.
    pub(crate) fn install_act(
        shell: &mut Shell,
        name: &'static str,
        act: impl Fn(&Mooring, &mut Shell) + Send + Sync + 'static,
    ) {
        use crate::typecheck::builtins::{mk_scheme, pure, thunk};
        let entry = crate::types::BuiltinEntry::new(
            std::borrow::Cow::Borrowed(name),
            |_| mk_scheme(&[], &[], thunk(pure(crate::ty::Ty::Unit))),
            "test-only: act from inside the run.",
            crate::types::BuiltinBody::Captured(Arc::new(move |_, mooring, shell| {
                act(mooring, shell);
                Ok(crate::types::Value::Unit)
            })),
        );
        shell.install_captured_builtins(&vec![entry].into());
    }

    /// An act striking its own run with `cause`, as a `Control` does.
    fn strike(cause: crate::process::CancelCause) -> impl Fn(&Mooring, &mut Shell) + Send + Sync {
        move |mooring, _| mooring.cancel.cancel(cause)
    }

    #[test]
    fn clean_run_settles_with_zero_status() {
        let mut shell = crate::test_helper::core_shell();
        match shell.run(Run::captured("$[1 + 1]", "<test>")) {
            RunReport::Ran { ending, .. } => {
                assert_eq!(ending.status(), 0);
                let result = ending.into_result();
                assert!(result.is_ok(), "expected Ok, got {result:?}");
            }
            RunReport::Static { .. } => panic!("clean source must not be Static"),
        }
    }

    #[test]
    fn parse_failure_is_static() {
        let mut shell = crate::test_helper::core_shell();
        match shell.run(Run::captured("let = ", "<test>")) {
            RunReport::Static { diagnostics } => {
                assert!(
                    matches!(
                        diagnostics,
                        StaticDiagnostics::Compile(Rejection { status: 2, .. })
                    ),
                    "expected a parse diagnostic"
                );
            }
            RunReport::Ran { .. } => panic!("malformed source must be Static"),
        }
    }

    /// A hook's program is an already-compiled value, so `run_framed` has no
    /// text to register. The registry never shrinks, so an entry naming the
    /// empty string would stand for the session's whole life — one per prompt
    /// draw, and one per keystroke under a `buffer-change` hook.
    #[test]
    fn a_hook_run_registers_no_source() {
        let mut shell = crate::test_helper::core_shell();
        shell.run(Run::captured("let body = { 1 }", "<test>"));
        let thunk = shell
            .scope_lookup("body")
            .cloned()
            .expect("body must be bound");
        let name = crate::types::HookName::session("test_hook");
        shell
            .register_hook(
                name.clone(),
                thunk,
                crate::types::HookSig::Prompt,
                crate::types::DefaultPolicy::denied(),
            )
            .expect("register the hook");

        let before = shell.sources().next_id();
        let report = shell.run(Run {
            program: Program::Hook { name, args: vec![] },
            ..Run::captured("", "<test>")
        });
        assert!(
            matches!(report, RunReport::Ran { .. }),
            "the registered hook must run"
        );
        assert_eq!(
            shell.sources().next_id(),
            before,
            "a hook run must mint no source id"
        );
    }

    /// Register the block `src` as hook `name` under `policy`, and run it.
    fn run_block_hook(
        shell: &mut Shell,
        name: crate::types::HookName,
        src: &str,
        policy: crate::types::DefaultPolicy,
    ) -> Ending {
        shell.run(Run::captured(format!("let hook_body = {src}"), "<test>"));
        let body = shell
            .scope_lookup("hook_body")
            .cloned()
            .expect("hook_body is bound");
        shell
            .register_hook(name.clone(), body, crate::types::HookSig::Prompt, policy)
            .expect("register the hook");
        let RunReport::Ran { ending, .. } = shell.run(Run {
            program: Program::Hook { name, args: vec![] },
            ..Run::captured("", "<test>")
        }) else {
            panic!("the registered hook must run");
        };
        ending
    }

    /// A hook run's fault names its hook, so no host needs a wrapper.
    #[test]
    fn a_hook_fault_names_its_hook() {
        let deny = crate::types::DefaultPolicy::denied;
        for (name, label) in [
            (crate::types::HookName::session("prompt"), "prompt: "),
            (
                crate::types::HookName::plugin("p", "h"),
                "plugin 'p' hook 'h': ",
            ),
        ] {
            let mut shell = crate::test_helper::core_shell();
            let Ending::Raised { error, .. } =
                run_block_hook(&mut shell, name, "{ cd '' }", deny())
            else {
                panic!("`cd ''` must raise");
            };
            assert!(error.message.starts_with(label), "{}", error.message);
        }
    }

    /// An aside hook runs beside the session: its `cd` dies with it.
    #[test]
    fn an_aside_hook_leaves_the_session_where_it_was() {
        let dir = tempfile::tempdir().expect("a tempdir");
        for (policy, moved) in [
            (crate::types::DefaultPolicy::denied(), true),
            (crate::types::DefaultPolicy::denied().aside(), false),
        ] {
            let mut shell = crate::test_helper::core_shell();
            shell.seed_cwd(dir.path().to_path_buf());
            let start = shell.cwd();
            let name = crate::types::HookName::session("buffer-change");
            let ending = run_block_hook(&mut shell, name, "{ cd .. }", policy);
            assert!(matches!(ending, Ending::Settled { .. }), "{ending:?}");
            assert_eq!(shell.cwd() != start, moved);
        }
    }

    #[test]
    fn type_failure_is_static() {
        let mut shell = crate::test_helper::core_shell();
        match shell.run(Run::captured("$[1 + true]", "<test>")) {
            RunReport::Static { diagnostics } => {
                assert!(
                    matches!(
                        diagnostics,
                        StaticDiagnostics::Compile(Rejection { status: 1, .. })
                    ),
                    "expected type diagnostics"
                );
            }
            RunReport::Ran { .. } => panic!("ill-typed source must be Static"),
        }
    }

    #[test]
    fn exit_escape_reports_code() {
        let mut shell = crate::test_helper::core_shell();
        match shell.run(Run::captured("exit 3", "<test>")) {
            RunReport::Ran { ending, .. } => {
                assert_eq!(ending.status(), 3);
                let result = ending.into_result();
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
        let mut shell = crate::test_helper::core_shell();
        match shell.run(Run::captured("echo hi", "<test>")) {
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
        let mut shell = crate::test_helper::core_shell();
        let pre = shell.session.anchor.clone();
        assert!(!pre.is_cancelled(), "pre-run anchor is live");

        let _ = shell.run(RunRequest {
            surface: Some(Arc::new(())),
            ..RunRequest::from(Run::captured("$[1 + 1]", "<test>"))
        });

        assert!(
            !shell.session.anchor.is_cancelled(),
            "a settled run must not cancel the anchor it hung off"
        );
        shell
            .session
            .anchor
            .cancel(crate::process::CancelCause::Cancelled);
        assert!(
            pre.is_cancelled(),
            "the anchor must still be the pre-run scope, not the run's own child"
        );
    }

    /// The host's own `Nursery` clone is empty afterward: teardown reaches
    /// through the clone, not just the run's copy.
    #[test]
    fn nursery_is_emptied_at_run_teardown() {
        let mut shell = crate::test_helper::core_shell();

        let nursery = crate::types::Nursery::default();
        let parked_id: Arc<Mutex<Option<crate::types::NurseryId>>> = Arc::new(Mutex::new(None));
        let parked = parked_id.clone();
        install_act(&mut shell, "park-fork", move |mooring, shell| {
            let id = shell
                .fork_into_nursery(mooring)
                .expect("a nursery is installed on this run");
            *parked.lock().unwrap() = Some(id);
        });

        let _ = shell.run(RunRequest {
            fork: Some(crate::types::Fork::Park(nursery.clone())),
            ..RunRequest::from(Run::captured("park-fork", "<test>"))
        });

        let id = parked_id
            .lock()
            .unwrap()
            .expect("the run must fork into the nursery");
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
        let mut shell = crate::test_helper::core_shell();

        // Never polled, under a millisecond-scale idle lease, so the background
        // lease chain reaps it quickly.
        let mut req = Run::captured("spawn { sleep 10 }", "<test>");
        req.deferred_lease = Some(crate::types::WorkerLease {
            idle: std::time::Duration::from_millis(20),
            backstop: std::time::Duration::from_secs(10),
        });
        let _ = shell.run(req);

        crate::test_helper::eventually(std::time::Duration::from_secs(2), || {
            (shell.worker_count() == 0).then_some(())
        })
        .expect("the unpolled worker must be reaped within the budget");

        // The first live sink of the session: until this run, the pending reap
        // has nowhere to push through.
        let notices = surfaced_notices(&mut shell, "$[1 + 1]");
        assert!(
            matches!(notices.as_slice(), [crate::types::Notice::Reap(_)]),
            "the settled run must surface the pending reap as a `notice`, got {notices:?}"
        );
    }

    /// An idle top-level name is pruned at the ready boundary of the run that
    /// crosses its bound, and announced there exactly once.
    #[test]
    fn ready_boundary_notice_surfaces_an_idle_prune() {
        let mut shell = crate::test_helper::core_shell();
        shell.arm_binding_lease(crate::types::BindingLease {
            idle_calls: 2,
            large_binding_bytes: u64::MAX,
        });

        let mut notices = surfaced_notices(&mut shell, "let idle_x = 1");
        notices.extend(surfaced_notices(&mut shell, "$[0]"));
        notices.extend(surfaced_notices(&mut shell, "$[0]"));
        let [crate::types::Notice::Prune(pruned)] = notices.as_slice() else {
            panic!("exactly one prune across the bound, got {notices:?}");
        };
        assert!(
            matches!(pruned.as_slice(), [p] if p.name == "idle_x"),
            "the prune names the idle binding, got {pruned:?}"
        );

        assert!(
            surfaced_notices(&mut shell, "$[0]").is_empty(),
            "nothing left idle: no second prune"
        );
    }

    /// Run `src` under a capturing surface sink, answering the notices it pushed.
    fn surfaced_notices(shell: &mut Shell, src: &str) -> Vec<crate::types::Notice> {
        struct CapturingSink(Arc<Mutex<Vec<crate::first_order::FOValue>>>);
        impl crate::types::EventSink for CapturingSink {
            fn emit(&self, ev: &crate::first_order::FOValue) {
                self.0.lock().unwrap().push(ev.clone());
            }
        }
        let captured: Arc<Mutex<Vec<crate::first_order::FOValue>>> =
            Arc::new(Mutex::new(Vec::new()));
        let _ = shell.run(RunRequest {
            surface: Some(Arc::new(CapturingSink(captured.clone()))),
            ..RunRequest::from(Run::captured(src, "<test>"))
        });
        let events = captured.lock().unwrap().clone();
        events
            .iter()
            .filter_map(crate::types::Notice::from_surface)
            .collect()
    }

    /// An interrupt struck mid-run is read at the next poll point, so
    /// `Mooring::check` unwinds the eval at 130.
    #[test]
    fn an_interrupt_mid_run_unwinds_the_eval() {
        let mut shell = crate::test_helper::core_shell();
        install_act(
            &mut shell,
            "interrupt",
            strike(crate::process::CancelCause::Interrupted),
        );
        match shell.run(Run::captured(
            "interrupt\nlet rpv = 42\nreturn $rpv",
            "<test>",
        )) {
            RunReport::Ran { ending, .. } => {
                assert_eq!(
                    ending.status(),
                    130,
                    "a foreground cancel observed mid-eval reports 130"
                );
                let result = ending.into_result();
                assert!(
                    matches!(result, Err(Break::Error(_))),
                    "the cancel must unwind into a Break::Error, got {result:?}"
                );
            }
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// `try` classifies a cancellation like any recoverable error, so a handler
    /// that settles cleanly makes the outcome `Ok`, whose status is 0 — the
    /// run boundary must re-poll the monotone scope before the status is
    /// read, or a final `try { … } { |_| return () }` suppresses the
    /// interrupt entirely.
    #[test]
    fn a_try_handler_cannot_settle_a_cancelled_run() {
        let mut shell = crate::test_helper::core_shell();
        install_act(
            &mut shell,
            "interrupt",
            strike(crate::process::CancelCause::Interrupted),
        );
        match shell.run(Run::captured(
            "try { interrupt\nlet x = ()\nreturn $x } { |_| return () }",
            "<test>",
        )) {
            RunReport::Ran { ending, .. } => {
                assert_eq!(
                    ending.status(),
                    130,
                    "a try handler must not settle a cancelled run at 0"
                );
                match ending.into_result() {
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

    /// An aside — a separate `Shell` beside the session, as the REPL runs plugin
    /// hooks ([`Shell::join_session`]) — can neither absorb an interrupt aimed
    /// at the session's run nor withhold it: the aside mints its frames under
    /// its own anchor, off the struck run's chain.
    #[test]
    fn an_aside_cannot_absorb_its_callers_interrupt() {
        let mut shell = crate::test_helper::core_shell();
        let aside = Mutex::new(shell.join_session());
        let hook_status = Arc::new(Mutex::new(None));
        let status = hook_status.clone();
        install_act(&mut shell, "interrupt-then-aside", move |mooring, _| {
            mooring
                .cancel
                .cancel(crate::process::CancelCause::Interrupted);
            let report = aside
                .lock()
                .unwrap()
                .run(Run::captured("let y = 1\nreturn $y", "<test>"));
            match report {
                RunReport::Ran { ending, .. } => *status.lock().unwrap() = Some(ending.status()),
                RunReport::Static { .. } => panic!("valid source must reach evaluation"),
            }
        });
        match shell.run(Run::captured(
            "interrupt-then-aside\nlet rpv = 42\nreturn $rpv",
            "<test>",
        )) {
            RunReport::Ran { ending, .. } => assert_eq!(
                ending.status(),
                130,
                "the interrupt must survive the aside's entry and unwind the run it was aimed at"
            ),
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
        assert_eq!(
            *hook_status.lock().unwrap(),
            Some(0),
            "while the aside's own run, off the struck chain, is deaf to it"
        );
    }

    /// The other half: an interrupt raised *while* the aside runs does unwind
    /// it, so plugin code is interruptible for its whole life.
    #[test]
    fn an_aside_unwinds_on_an_interrupt_raised_while_it_runs() {
        let shell = crate::test_helper::core_shell();
        let mut aside = shell.join_session();
        install_act(
            &mut aside,
            "interrupt",
            strike(crate::process::CancelCause::Interrupted),
        );
        match aside.run(Run::captured("interrupt\nlet y = 1\nreturn $y", "<test>")) {
            RunReport::Ran { ending, .. } => assert_eq!(
                ending.status(),
                130,
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
        let shell = crate::test_helper::core_shell();
        let mut aside = shell.join_session();

        shell
            .cancel_handle()
            .cancel(crate::process::CancelCause::Cancelled);

        match aside.run(Run::captured("let y = 1\nreturn $y", "<test>")) {
            RunReport::Ran { ending, .. } => {
                assert_eq!(
                    ending.status(),
                    143,
                    "the aside's run unwinds with the session"
                );
                let result = ending.into_result();
                assert!(
                    matches!(result, Err(Break::Error(_))),
                    "the handle cancel must unwind the aside, got {result:?}"
                );
            }
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// No session folds the ambient causes: a signal reaches a run only as
    /// the `Control` its host forwards, so one raised mid-run with nobody
    /// listening leaves the eval undisturbed.
    #[test]
    fn a_session_is_deaf_to_the_ambient_causes() {
        let _serial = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = crate::test_helper::core_shell();
        install_act(&mut shell, "raise", |_, _| {
            crate::process::request_interrupt();
            crate::process::request_root_cancel(crate::process::CancelCause::Aborted);
        });
        let report = shell.run(Run::captured("raise\nlet rpv = 42\nreturn $rpv", "<test>"));
        crate::process::cancel::clear_root_request();
        match report {
            RunReport::Ran { ending, .. } => {
                assert_eq!(
                    ending.status(),
                    0,
                    "an ambient cause must not reach the run"
                );
                assert!(ending.into_result().is_ok());
            }
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// The act fires the `Deadline` cause directly, pinning the `Walled`
    /// classification without sleeping on the reaper.
    #[test]
    fn deadline_cancel_reports_walled() {
        let mut shell = crate::test_helper::core_shell();
        install_act(
            &mut shell,
            "expire",
            strike(crate::process::CancelCause::TimedOut),
        );
        match shell.run(Run {
            wall: Some(std::time::Duration::from_secs(30)),
            ..Run::captured("expire\nlet rpv = 42\nreturn $rpv", "<test>")
        }) {
            RunReport::Ran { ending, .. } => {
                assert!(
                    matches!(ending, Ending::Walled { .. }),
                    "a Deadline foreground cancel must report Walled"
                );
            }
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// A real wall, so the cancel lands *during* the script: in
    /// `deadline_cancel_reports_walled` it fires ahead of every binding, so
    /// none has landed there for the wall to be measured against.
    #[test]
    fn mid_script_wall_keeps_the_bindings_that_landed() {
        let mut shell = crate::test_helper::core_shell();
        match shell.run(Run {
            wall: Some(std::time::Duration::from_millis(500)),
            ..Run::captured("let pre_wall = 1\nsleep 30\nlet post_wall = 2", "<test>")
        }) {
            RunReport::Ran { ending, .. } => assert!(
                matches!(ending, Ending::Walled { .. }),
                "a wall cut mid-script must report Walled, got {ending:?}"
            ),
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
        assert!(
            shell.scope_lookup("pre_wall").is_some(),
            "the `let` that landed before the wall stays bound in the session"
        );
        assert!(
            shell.scope_lookup("post_wall").is_none(),
            "the `let` after the wall never ran"
        );
    }

    /// Under `RunIo::Inherit` the guard restores the session's stdout sink to
    /// the *same* object it was before the run, and the run's output lands in
    /// it.
    #[test]
    fn inherit_leaves_session_streams_untouched() {
        let mut shell = crate::test_helper::core_shell();
        let (marker_sink, marker) = crate::io::new_buffer();
        shell.io.stdout = marker_sink;

        let _ = shell.run(Run {
            io: RunIo::Inherit,
            ..Run::captured("echo hi", "<test>")
        });

        assert!(
            matches!(&shell.io.stdout, Sink::Buffer(b) if Arc::ptr_eq(b, &marker)),
            "Inherit must restore the session's stdout sink after the run"
        );
        let written = marker.peek();
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
    // fields already rolled back to their run-entry state.

    /// Stands in for any Rust panic the evaluator can raise mid-run.
    fn builtin_panic_now(
        _args: &[crate::types::Value],
        _mooring: &Mooring,
        _shell: &mut Shell,
    ) -> crate::types::Settled<crate::types::Value> {
        panic!("run-door test: deliberate mid-eval panic");
    }

    fn scheme_panic_now(_u: &mut crate::typecheck::Unifier) -> crate::ty::Scheme {
        use crate::typecheck::builtins::{mk_scheme, pure, thunk};
        mk_scheme(&[], &[], thunk(pure(crate::ty::Ty::Unit)))
    }

    static PANIC_BUILTINS_ARR: [crate::types::BuiltinEntry; 1] = [crate::types::BuiltinEntry::new(
        std::borrow::Cow::Borrowed("core-panic-now"),
        scheme_panic_now,
        "test-only: panic the evaluator mid-run.",
        crate::types::BuiltinBody::Static(builtin_panic_now),
    )];
    static PANIC_BUILTINS: &[crate::types::BuiltinEntry] = &PANIC_BUILTINS_ARR;

    /// Rollback is to the run's entry, not to session birth: the panicking run's
    /// partial binding goes, a pre-run one stays, and the shell runs on.
    #[test]
    fn panicking_run_reports_failed_and_rolls_back() {
        let mut shell = crate::test_helper::core_shell();
        shell.install_builtins(PANIC_BUILTINS);

        match shell.run(Run::captured("let pre_panic = 1", "<test>")) {
            RunReport::Ran { ending, .. } => assert!(matches!(ending, Ending::Settled { .. })),
            RunReport::Static { .. } => panic!("the pre-run binding must evaluate"),
        }

        match shell.run(Run::captured("let mid_panic = 2\ncore-panic-now", "<test>")) {
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

        match shell.run(Run::captured("$pre_panic", "<test>")) {
            RunReport::Ran { ending, .. } => {
                assert!(
                    matches!(ending, Ending::Settled { .. }),
                    "the next run must evaluate clean on the same shell"
                );
            }
            RunReport::Static { .. } => panic!("the healed shell must still evaluate"),
        }
    }

    /// A host-supplied handler panicking mid-enquiry unwinds back through the
    /// run, so the door catches it like any other.
    struct PanickingDesk;
    impl crate::types::EnquiryDesk for PanickingDesk {
        fn enquire(
            &self,
            _req: crate::first_order::FOValue,
            _cancel: &crate::process::CancelScope,
        ) -> Result<crate::first_order::FOValue, crate::types::Error> {
            panic!("run-door test: desk handler panic");
        }
    }

    #[test]
    fn desk_handler_panic_is_caught_at_the_door() {
        let mut shell = crate::test_helper::core_shell();
        install_act(&mut shell, "enquire", |mooring, shell| {
            let _ = shell.enquire(mooring, crate::first_order::FOValue::Unit);
        });

        match shell.run(RunRequest {
            desk: Some(Arc::new(PanickingDesk)),
            ..RunRequest::from(Run::captured("let desk_panic = 4\nenquire", "<test>"))
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
        let report = shell.run(Run::captured(src, "<test>")).into_report(shell);
        let crate::protocol::Report::Ran { ending, .. } = report else {
            panic!("{src:?} must reach evaluation");
        };
        match ending {
            crate::protocol::Ending::Raised { rendered, .. }
            | crate::protocol::Ending::Walled { rendered, .. } => rendered,
            other => panic!("{src:?} must fault, got {other:?}"),
        }
    }

    /// A result the wire cannot carry names what it is or holds, and the
    /// remedy that kind of value suggests.
    #[test]
    fn a_result_that_is_not_data_says_what_it_holds() {
        let mut shell = crate::test_helper::core_shell();
        for (src, says, hint) in [
            ("spawn { echo hi }", "the result is a handle", "let h ="),
            ("{ echo hi }", "the result is a block", "!{ … }"),
            ("{ |x| echo $x }", "the result is a function", "arguments"),
            (
                "[n: 1, h: !{spawn { echo hi }}]",
                "the result holds a handle",
                "let h =",
            ),
            ("[k: { echo hi }]", "the result holds a block", "!{ … }"),
        ] {
            let report = shell.run(Run::captured(src, "<test>")).into_report(&shell);
            let crate::protocol::Report::Ran { ending, .. } = report else {
                panic!("{src:?} must reach evaluation");
            };
            let crate::protocol::Ending::Unreturnable { rendered, .. } = &ending else {
                panic!("{src:?} must end unreturnable, got {ending:?}");
            };
            assert!(rendered.contains(says), "{src:?}: {rendered:?}");
            assert!(rendered.contains(hint), "{src:?}: {rendered:?}");
            assert_eq!(ending.status(), 1, "{src:?} must not report success");
        }
    }

    /// A shell whose `_narrow` pushes a `net: false` session ceiling and whose
    /// `_narrowed` reports whether any ceiling holds.
    fn narrowing_shell() -> Shell {
        use crate::capability::Capabilities;
        use crate::types::{BuiltinBody, BuiltinEntry};
        let mut shell = crate::test_helper::core_shell();
        let entries: Arc<[BuiltinEntry]> = vec![
            BuiltinEntry::new(
                "_narrow".into(),
                crate::typecheck::builtins::scheme::pure_bool,
                "",
                BuiltinBody::Captured(Arc::new(|_, _, s: &mut Shell| {
                    s.push_session_capabilities(Capabilities {
                        net: Some(false),
                        ..Capabilities::root()
                    });
                    Ok(Value::Bool(true))
                })),
            ),
            BuiltinEntry::new(
                "_narrowed".into(),
                crate::typecheck::builtins::scheme::pure_bool,
                "",
                BuiltinBody::Captured(Arc::new(|_, _, s: &mut Shell| {
                    Ok(Value::Bool(s.has_active_capabilities()))
                })),
            ),
        ]
        .into();
        shell.install_captured_builtins(&entries);
        shell
    }

    /// Run `narrow`, then assert its session ceiling alone outlived it and
    /// narrows the next run.
    fn assert_ceiling_survives(mut shell: Shell, narrow: Run) {
        let before = shell.context.grants.len();
        assert!(matches!(shell.run(narrow), RunReport::Ran { .. }));
        assert_eq!(
            shell.context.grants.len(),
            before + 1,
            "the run's own frames must leave with it, and only they"
        );
        assert_eq!(shell.context.grants.net().collect::<Vec<_>>(), [false]);
        let RunReport::Ran {
            ending: Ending::Settled { value, .. },
            ..
        } = shell.run(Run::captured("_narrowed", "<test>"))
        else {
            panic!("the probe run must settle");
        };
        assert!(
            matches!(value, Value::Bool(true)),
            "the ceiling must narrow the next run"
        );
    }

    /// A session ceiling pushed inside a run outlives it and narrows the next,
    /// while the run's own `Run.caps` frames leave with it.
    #[test]
    fn a_session_ceiling_pushed_in_a_run_outlives_it() {
        let mut caps = GrantStack::root();
        caps.push(crate::capability::Capabilities {
            detach: Some(false),
            ..crate::capability::Capabilities::root()
        });
        let mut narrow = Run::captured("_narrow", "<test>");
        narrow.caps = caps;
        assert_ceiling_survives(narrowing_shell(), narrow);
    }

    /// The same inside a `grant { }` block: its frame leaves by position, not
    /// by count, so the ceiling pushed above it stays.
    #[test]
    fn a_session_ceiling_pushed_in_a_grant_block_outlives_it() {
        assert_ceiling_survives(
            narrowing_shell(),
            Run::captured("grant [net: true] { _narrow }", "<test>"),
        );
    }

    /// A lambda compiled by one run and called by the next draws its caret into
    /// the text that defined it: the registry only grows, so a run boundary
    /// costs a value nothing of its origin.
    #[test]
    fn a_lambda_faults_against_the_run_that_compiled_it() {
        let mut shell = crate::test_helper::core_shell();
        assert!(matches!(
            shell.run(Run::captured(
                "let boom = { |x| fail [status: 1, message: nope] }",
                "<test>"
            )),
            RunReport::Ran {
                ending: Ending::Settled { .. },
                ..
            }
        ));
        let rendered = crate::ansi::strip(&rendered_fault(&mut shell, "boom 1"));
        assert!(
            rendered.contains("message: nope] }"),
            "the caret must be drawn into the defining run's text:\n{rendered}"
        );
    }

    /// A fault in text the user can already see needs no header and no caret.
    #[test]
    fn a_single_command_faulting_in_its_own_text_renders_compact() {
        let mut shell = crate::test_helper::core_shell();
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
        let mut shell = crate::test_helper::core_shell();
        for (src, shape) in [
            ("no-such-command-xyz", true),
            ("no-such-command-xyz; true", false),
        ] {
            match shell.run(Run::captured(src, "<test>")) {
                RunReport::Ran {
                    ending: Ending::Raised { compact, .. },
                    ..
                } => {
                    assert_eq!(compact.is_some(), shape, "{src:?} misreports its shape");
                }
                RunReport::Ran { ending, .. } => panic!("{src:?} must fault, got {ending:?}"),
                RunReport::Static { .. } => panic!("{src:?} must reach evaluation"),
            }
        }
    }

    // ── The dispatch's own trail ─────────────────────────────────────────

    /// `Run.trail: Some` opens a scope at `enter`, closes it into the
    /// `RunReport`, and `into_report` projects each observation through
    /// `encode` onto the wire — the whole path a dispatching host takes.
    #[test]
    fn a_dispatch_trail_projects_and_round_trips_through_the_wire() {
        let mut shell = crate::test_helper::core_shell();
        let report = shell.run(Run {
            trail: Some(crate::types::CapturePolicy::Off),
            ..Run::captured("/bin/echo hi", "<test>")
        });
        let crate::protocol::Report::Ran { trail, .. } = report.into_report(&shell) else {
            panic!("valid source must reach evaluation");
        };
        assert!(
            !trail.is_empty(),
            "the dispatch's own trail must carry its one command"
        );
        let round_tripped = trail.iter().any(|fo| {
            let obs = crate::first_order::datum::Datum::decode(fo);
            matches!(
                obs,
                Ok(crate::types::Observation {
                    what: crate::types::Observed::Command(_),
                    ..
                })
            )
        });
        assert!(
            round_tripped,
            "a wire `FOValue` must decode back as the `Command` observation it was"
        );
    }

    /// `Run.trail: None` neither opens a scope nor collects one — the REPL's
    /// choice, and every other test's, `Run::captured` included.
    #[test]
    fn a_dispatch_asking_no_trail_reports_none() {
        let mut shell = crate::test_helper::core_shell();
        match shell.run(Run::captured("$[1 + 1]", "<test>")) {
            RunReport::Ran { trail, .. } => assert!(
                trail.is_empty(),
                "an unasked dispatch must report no trail at all"
            ),
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }

    /// A panicked dispatch is excluded from the trail law by declaration: it
    /// reports `Static`, and the scope `enter` held — outside the
    /// `catch_unwind` the panic escapes through — still drains and closes,
    /// discarded rather than attached. The next dispatch that asks opens a
    /// fresh scope and sees nothing left over.
    #[test]
    fn a_panicked_dispatch_reports_static_and_leaves_the_next_trail_empty() {
        let mut shell = crate::test_helper::core_shell();
        shell.install_builtins(PANIC_BUILTINS);

        match shell.run(Run {
            trail: Some(crate::types::CapturePolicy::Off),
            ..Run::captured("core-panic-now", "<test>")
        }) {
            RunReport::Static {
                diagnostics: StaticDiagnostics::Host(e),
            } => assert!(e.message.contains("run panicked")),
            _ => panic!("a panicking dispatch must report Static{{Host}}"),
        }

        match shell.run(Run {
            trail: Some(crate::types::CapturePolicy::Off),
            ..Run::captured("$[1 + 1]", "<test>")
        }) {
            RunReport::Ran { trail, .. } => assert!(
                trail.is_empty(),
                "the panicked dispatch's scope must not leak into the next one"
            ),
            RunReport::Static { .. } => panic!("valid source must reach evaluation"),
        }
    }
}
