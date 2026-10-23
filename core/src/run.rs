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

use crate::compile::compile_and_typecheck;
use crate::diagnostic::{Rejection, Report};
use crate::io::{Captured, Io, RunIo, RunStdin, Sink, Source};
use crate::process::{CancelCause, Deadline, RequestedTerminalAccess, Scope, ScopeNode};
use crate::protocol::{Program, Run};
use crate::source::{FileId, Span};
use crate::types::{
    Break, DeferredSink, Desk, Error, Escape, Fork, Mooring, Notice, NurseryGuard, Observation,
    Settled, Shell, SurfaceSink, TrailScope, Value, final_poll,
};
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
            Self::Host(e) => render_spanless(e),
        }
    }
}

/// A spanless [`Error`] drawn as the one-liner, with the exit status it ends
/// on: the one rule for an error that has no source to point a caret into.
fn render_spanless(e: &Error) -> (String, i32) {
    let report = Report {
        code: None,
        message: e.message.clone(),
        at: None,
        also: None,
        hint: e.hint.as_deref().map(Into::into),
    };
    (report.plain(), e.code())
}

// ── The run entry: one synchronous, runtime-agnostic host entry ──────────────
//
// Hosts describe *policy*; core owns *resources*. The reduction primitive
// behind the entry is crate-private, so no host can start an unframed
// evaluation that would foreground or capture against a stale frame.

/// The engine entry for one run: the protocol [`Run`] plus the live,
/// non-transportable handles the host lends it.
///
/// Composition, not mirroring — a field added to [`Run`] reaches the engine
/// in one declaration.
pub struct RunRequest {
    pub run: Run,
    /// Run-local sink for structured events; `None` is the identity.
    pub surface: Option<SurfaceSink>,
    /// Session-lived destination a settling worker delivers its surface batch
    /// to; `None` drops a worker's surface.
    pub deferred: Option<Arc<dyn DeferredSink>>,
    /// Run-local enquiry desk. Same-thread children inherit it, as they do the
    /// fork handle below; deferred workers get neither.
    pub desk: Option<Desk>,
    /// How a forked session reaches this run's desk; `None` adopts none.
    pub fork: Option<Fork>,
}

impl From<Run> for RunRequest {
    /// A request lending the host's run nothing: no surface, deferred sink,
    /// desk or fork handle.
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
    /// The run never reached evaluation, so no command ran and nothing was
    /// bound; the host renders the diagnostics and exits on the status they
    /// carry.
    Static { diagnostics: StaticDiagnostics },
    Ran {
        ending: Ending,
        captured: Option<Captured>,
        /// This dispatch's own trail, filled in by [`Shell::enter`] once the
        /// scope it held closes; empty when `Run.trail` was `None`.
        trail: Vec<Observation>,
    },
    /// The run panicked. `env` and `context` were rolled back to the
    /// checkpoint taken at entry, but whatever the run did outside them
    /// (commands, files) stands.
    Faulted { error: Error },
}

#[cfg(any(test, feature = "test-util"))]
impl RunReport {
    /// The ending of a run whose source must reach evaluation and report;
    /// a static rejection or a fault panics, naming its diagnostic.
    ///
    /// # Panics
    /// On [`Self::Static`] and [`Self::Faulted`].
    #[track_caller]
    pub fn expect_ran(self) -> Ending {
        match self {
            Self::Ran { ending, .. } => ending,
            Self::Static { diagnostics } => panic!(
                "the run must reach evaluation, but was refused:\n{}",
                diagnostics.render().0
            ),
            Self::Faulted { error } => panic!(
                "the run must reach evaluation and report, but faulted: {}",
                error.message
            ),
        }
    }
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
    /// A stop that reached the top, a top-level effect: the run was ended, not failed.
    Stopped {
        cause: CancelCause,
        at: Option<Span>,
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
            Self::Raised { error, .. } => error.code(),
            Self::Stopped { cause, .. } => i32::from(cause.code()),
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
            Self::Raised { error, .. } => Err(Break::Error(error)),
            Self::Stopped { cause, at, .. } => Err(Break::Escape(Escape::Stopped {
                cause,
                owner: None,
                at,
            })),
            Self::Exited(code) => Err(Break::Escape(Escape::Exit(code))),
        }
    }
}

/// Bind the host's requested `wall` to this run's foreground scope, so the
/// deadline reaches every frame nested under it.
fn arm_wall(
    wall: std::time::Duration,
    foreground: &crate::process::ScopeNode,
) -> crate::process::Deadline {
    crate::process::arm_lifetime(foreground.clone(), wall)
}

impl Shell {
    /// Run one whole [`Run`] synchronously and report it — the run entry for a
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
    /// entry a host takes when it must be able to cancel this run from another
    /// thread, including before the run has begun.  [`Shell::run`] is this entry
    /// with the anchor.
    ///
    /// A dispatch that asks for a trail (`req.run.trail: Some`) gets its
    /// scope held *here*, outside the `catch_unwind` below: the checkpoint
    /// `dispatch`'s panic arm rolls back does not cover `local.audit`
    /// (`LocalState` partitions it deliberately), so only a scope the panic
    /// cannot skip keeps the opener's close law true at dispatch
    /// granularity. A panic reports `Faulted` by declaration — the trail
    /// closes and is discarded, never attached.
    pub(crate) fn run_under(&mut self, under: &Scope, req: RunRequest) -> RunReport {
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
                RunReport::Faulted {
                    error: Error::raised(
                        format!(
                            "run panicked: {}",
                            crate::types::panic_text(payload.as_ref())
                        ),
                        101,
                    ),
                }
            }
        }
    }

    /// Resolve the run's [`Program`] and hand it to the framed scaffold, with
    /// this run's foreground frame minted under `under`.
    fn dispatch(&mut self, under: &Scope, mut req: RunRequest) -> RunReport {
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

                let (top, compact) = match compile_run(self, src, &req.run.origin) {
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
                let Some(hook) = self.hook(name).cloned() else {
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
        foreground: ScopeNode,
        wall: Option<Deadline>,
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
            home: foreground,
            cleanup_grace: run.cleanup_grace,
            worker_cap: run.worker_cap,
            worker_title: run.worker_title,
            terminal_access,
        };
        let next = build_run(self, capture.as_ref(), stdin);
        let (result, status) = run_framed(&mooring, self, next, &run, wall, body);

        RunReport::Ran {
            ending: classify_ending(result, status, compact),
            captured: capture.map(Capture::finish),
            // Filled in by `enter`, which holds this dispatch's scope.
            trail: Vec::new(),
        }
    }
}

/// Fold a settled body's raw parts into the one [`Ending`] they describe.
/// `status` is used only for [`Ending::Settled`] — every other arm's status
/// is a pure function of the escape, read lazily through [`Ending::status`].
fn classify_ending(result: Settled<Value>, status: i32, compact: Option<FileId>) -> Ending {
    match result {
        Ok(value) => Ending::Settled { value, status },
        Err(Break::Error(error)) => Ending::Raised { error, compact },
        Err(Break::Escape(Escape::Exit(code))) => Ending::Exited(code),
        Err(Break::Escape(Escape::Stopped { cause, at, .. })) => {
            Ending::Stopped { cause, at, compact }
        }
    }
}

// ── The spine behind the entry: compile, build, install, classify ────────────

/// Holds what a run takes on loan from the shell — the byte streams, the
/// current scope, `session.root_file`, `local.audit.call_site` — and restores it on `Drop`, so
/// an unwinding run leaves nothing stale on the persistent `Shell`. Everything
/// else a run installs lives on the [`Mooring`], which was never on the shell
/// and so needs no restoring.
struct IoLoan<'s> {
    shell: &'s mut Shell,
    saved: Io,
    saved_scope: Scope,
    saved_root: Option<FileId>,
    saved_site: Option<Span>,
}

impl<'s> IoLoan<'s> {
    /// The run starts with the registers cleared, not inherited: a fresh run
    /// has no call site and no root file until it registers one.
    fn install(shell: &'s mut Shell, next: Io, scope: Scope) -> Self {
        let saved = std::mem::replace(&mut shell.io, next);
        let saved_scope = std::mem::replace(&mut shell.scope, scope);
        let saved_root = shell.session.root_file.take();
        let saved_site = shell.local.audit.call_site.take();
        Self {
            shell,
            saved,
            saved_scope,
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
        std::mem::swap(&mut self.shell.scope, &mut self.saved_scope);
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
    origin: &crate::source::SourceOrigin,
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
    let outcome = compile_and_typecheck(src, schemes, file, origin.file(), None);
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
            error.reject(crate::source::Source::from_text(origin.clone(), src)),
        ))
    })?);

    let compact = top.is_single_command().then_some(file);
    Ok((top, compact))
}

/// Install the built run state, run `body` under `run`'s capabilities, compute the transport status, and tear the frame down. Both
/// program arms settle to one [`Settled<Value>`], so this spine is shared
/// verbatim; the caller keeps the mooring, the limits and the classification.
pub(crate) fn run_framed(
    mooring: &Mooring,
    shell: &mut Shell,
    next: Io,
    run: &Run,
    wall: Option<Deadline>,
    body: impl FnOnce(&Mooring, &mut Shell) -> Settled<Value>,
) -> (Settled<Value>, i32) {
    let guard = IoLoan::install(shell, next, Scope::clone(&mooring.home));
    let shell = &mut *guard.shell;
    // Only a source run registers: a hook's spans point into the file the hook
    // was defined in, so an entry for its absent text would name nothing and
    // never be reclaimed — the registry is append-only for the session's life.
    let boundary = match &run.program {
        Program::Source(src) => {
            shell.install_root_context(&run.origin, src);
            true
        }
        Program::Hook { .. } => false,
    };

    let mut caps = run.caps.clone();
    caps.rehold();
    let result = shell.with_layers(caps, |s| body(mooring, s));

    // The run's last step boundary.  The wall is disarmed first; a firing
    // the daemon has already claimed is waited for (the node may already
    // carry a weaker cause), so nothing fires after this poll.
    if let Some(claimed) = wall.and_then(Deadline::disarm) {
        claimed.wait();
    }
    let result = final_poll(result, &mooring.home);

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
            let _ = self.write_stderr(format!("{notice}\n").as_bytes());
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
#[allow(clippy::disallowed_methods, reason = "test code")]
pub(crate) mod tests;
