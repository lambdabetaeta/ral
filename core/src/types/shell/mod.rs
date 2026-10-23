//! Interpreter state, partitioned by lifetime: the field name *is* the
//! invariant.  `env` and `context` persist and cross evaluation boundaries
//! and thread spawns; `io` ([`Io`]) and `scope` belong to one run, swapped in
//! and restored on teardown by [`crate::run::IoLoan`], and within a run by the
//! frames that swap them; [`SessionState`] survives
//! every run; [`LocalState`] is host scratch whose members each carry their
//! own rule.
//!
//! What a run fixes *once* — where its events go, who answers it, what stops
//! it, its terminal authority — is not here at all: it is the [`Mooring`] on
//! the run's Rust stack frame, reaching callees as `&Mooring` disjoint from
//! `&mut Shell`.  Borrow what a run cannot change, loan what it can.
//!
//! [`Shell`]'s methods live in the submodules below, by concern; the primitives
//! too small to belong to one stay here.

pub(crate) mod bindings;
mod construct;
mod context;
pub(crate) mod cwd;
pub(crate) mod detached;
pub(crate) mod hooks;
mod host;
mod inherit;
pub(crate) mod modules;
pub(crate) mod notice;
mod scope;
mod scrub;
pub(crate) mod workers;

use self::bindings::BindingLedger;
pub use self::construct::{Bequest, Runtime};
pub(crate) use self::construct::{Image, Terms};
use self::detached::DetachPolicy;
use self::modules::Modules;
use self::workers::Roster;
use super::audit::Audit;
use super::env::Env;
use super::env::EnvVars;
use super::error::Error;
use super::flow::{Break, Settled};
use super::handler::HandlerStack;
use super::mooring::{Mooring, TerminalAccess};
use super::sig;
use crate::capability::GrantStack;
use crate::io::{Io, WriteStopped};
use crate::path::CwdPath;
use crate::process::{CancelCause, DurableRoot, Scope};
use crate::source::{CallSite, FileId, Source, SourceDb, SourceOrigin, Span};
use crate::types::Cradle;
use std::sync::Arc;
use std::time::Duration;

/// Default cap on the machine's stack: frames, not host stack frames, so it
/// can sit far above what a host stack allows.
pub const DEFAULT_STACK_LIMIT: usize = 100_000;

/// The Send+Clone dynamic context every child computation carries — a thunk
/// body, a spawned thread (`spawn`, `par`, pipeline stage), a REPL aside.
///
/// It clones wholesale into a child, so what is *not* here is the point: the
/// lexical environment flows by its own rule, the run's own frame is
/// installed afresh, and call site, builtin table and hooks stay run and
/// session state — so a clone carries no render registry and no host
/// dispatch.
///
/// `H` is the handler stack's form: the live [`HandlerStack`], or the seed's
/// interned frames, which is the only other shape the context takes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Context<H = HandlerStack> {
    // ── attenuable by within / grant ─────────────────────────────────────
    /// Process env-var overrides set by `within [env: …]`.  `PWD` is
    /// excluded, living on `cwd` below; `OLDPWD` too, as ral keeps no previous
    /// directory.
    pub(crate) env_overrides: EnvVars,
    /// The working-directory cell.  A fork copies it; a same-thread body
    /// shares it, so a `cd` there persists unless a `within [dir: …]` around
    /// it restores the cell.
    cwd: CwdPath,
    /// Capability restrictions, innermost last.
    pub(crate) grants: GrantStack,
    /// `within [handlers: …, handler: …]` effect-handler stack, innermost last.
    pub(crate) handlers: H,

    // ── dynamic context, not attenuable ─────────────────────────────────
    /// Invocation positionals (`args`), from the command line or `source`.
    pub(crate) args: Vec<String>,
    pub(crate) modules: Modules,
}

// The `capability::check_*(&Context, …)` decisions fold the whole stack from
// this one borrow, so the type system bars policy code from ever reading
// lexical scope, REPL scratch, control state, or exit hints.

/// State that outlives every run's teardown.
pub struct SessionState {
    /// A run's loose workers (`spawn`, `watch`) hang here, not under its
    /// swappable foreground scope, so a foreground cancel never reaches them.
    pub(crate) root: DurableRoot,
    /// The scope a run entered through [`Shell::run`](crate::Shell::run) nests
    /// its foreground frame under.
    pub(crate) anchor: Scope,
    /// Durable source registry, read by hosts *after* a run returns to render
    /// its runtime errors.  Append-only for the whole session, so a nested
    /// run's spans can never alias an outer run's [`FileId`].
    pub(crate) sources: SourceDb,
    /// The current run's root source, which a child inherits, so a bare `use`
    /// in a worker or a stage falls back to it.  `None` between runs: the
    /// fallback is then `""`, i.e. cwd-relative.
    pub(crate) root_file: Option<FileId>,
    pub(crate) exit_hints: Arc<crate::types::ExitHints>,
    /// The spelling of the directory the front end that booted the session
    /// made for it, which the `scratch` place of a grant anchors to, as `home`
    /// anchors to `HOME`: set once at boot, inherited by every child and
    /// carried over the wire with the session's terms.  `None` where the host
    /// made none, and then a `scratch` place is refused.
    pub(crate) scratch: Option<String>,
    /// What the session's `comparable` variables were fixed to, per unit.
    pub(crate) fixings: Arc<crate::ty::Fixings>,
    /// The binary's half, shared by every session of an engine. Builtin
    /// bodies are Rust fn pointers or captured host closures, hence
    /// process-local: the receiver of a wire shell installs its own.
    pub(crate) runtime: Arc<Runtime>,
    /// Minted once from the startup `tcgetpgrp == getpgrp` predicate: `Some`
    /// when ral owns the controlling terminal's foreground, `None` when piped,
    /// backgrounded, tty-less, or off Unix.  Lent — never moved or cloned — to
    /// the handoff, and only when the run's [`TerminalAccess`] permits.
    pub(crate) terminal_lease: Option<crate::process::TerminalLease>,
    /// The machine's stack cap: `Machine::reserve` refuses a push at
    /// `stack.len() >= stack_limit`.  Counts machine frames, not host stack
    /// frames; `--recursion-limit`/rc's `recursion-limit:` key set it.
    pub(crate) stack_limit: usize,
    /// Named run entry points (rc-declared prompt, startup block, plugin
    /// hooks): a namespace apart from both lexical scope and the handler
    /// stack, since hooks are run roots, never commands or variables.  Session
    /// state, not context: a worker or a stage never dispatches one, and a run
    /// that registers one and then panics does not roll it back.
    /// `None` for every child (aside, worker, stage, fork): only the booted
    /// session owns hooks and the plugin ledger that undoes them.
    pub(crate) hooks: Option<hooks::HookTable>,
}

/// Host-local scratch whose members each carry their own flow rule — not a
/// lifetime category, the residue left once run and session state are named.
pub struct LocalState {
    /// The in-flight audit trail and its byte-capture policy.  `grant`,
    /// `within`, `guard`, `try`, and `audit` are collection boundaries, not
    /// observations; the dispatcher posts one observation per real command
    /// through `types::audit::fan_out`.
    pub(crate) audit: Audit,
    /// Every worker (`spawn`, `watch`) spawned from this shell.
    /// [`Shell::spawn_thread`] shares the registry into the worker's own shell,
    /// so a nested `spawn` registers alongside its parent; a sub-agent fork
    /// starts fresh, one registry per agent.
    pub(crate) workers: Roster,
    /// Where a loose birth hangs and who settles it: loose under the root for
    /// a run and an aside, a worker's own node and brood inside a worker.  A
    /// stage shares its parent's.
    pub(crate) cradle: Cradle,
    /// Inert until a host arms it, then ages every non-baseline top-level name
    /// so long-unused scratch can be pruned.  Unlike [`Self::workers`] it needs
    /// no lock: its one writer is the thread owning `&mut Shell` for every run,
    /// install, and prune.  Workers and asides start inert; a fork is armed on
    /// its parent's terms.
    pub(crate) bindings: BindingLedger,
    /// Authority to birth processes outliving this session.  Armed in the same
    /// act that installs the `detach` builtin, so an unarmed shell lacks the
    /// verb rather than owning one it cannot budget.  `Arc`-shared into a
    /// worker's shell, so a `detach` inside `spawn { }` spends this budget; a
    /// fork gets its own, unspent.
    pub(crate) detach: Option<Arc<DetachPolicy>>,
    /// Nested `machine::evaluate`/`machine::apply` re-entries live on this
    /// host stack frame: a native such as `map` applying a user function.
    /// `machine::run` increments before its `catch_unwind` and decrements right
    /// after, so a panic unwinding out still leaves it lowered before the
    /// payload resumes.
    pub(crate) machine_depth: usize,
}

/// The runtime: a field changes within a run (`io`, `scope`), survives a run
/// ([`SessionState`]), crosses evaluation boundaries (`env`, `context`), or
/// is host scratch ([`LocalState`]).
///
/// A session is built by exactly two struct literals: `Shell::construct`, the
/// one constructor of a root or a child session, and `Shell::child`, for the
/// children that share a session's memory.
///
/// Every field is `pub(crate)`, because the partition encodes run safety,
/// capability attenuation, and wire framing — core's invariant, not an API a
/// host may reach past.  Hosts drive a session through the intent verbs
/// ([`mod@host`], plus the scope and context verbs), and the run entry
/// ([`Shell::run`]) checkpoints and rolls back `env` and `context` around
/// each run.
pub struct Shell {
    /// The session environment: the machine's focus environment between
    /// runs, extended by every `Define` that lands.
    pub(crate) env: Env,
    /// Σ: natives and the frozen prelude, one per shell, `Arc`-shared into
    /// every fork and spawned worker.
    pub(crate) sig: Arc<crate::types::Signature>,
    pub(crate) context: Context,
    pub(crate) io: Io,
    /// The scope the computation in focus polls: a thread base sets it to its
    /// mooring's node; `Frame::Cleanup` swaps it for a shield for a cleanup's
    /// extent and restores it, as `Frame::Capture` does `io.stdout`.  A view:
    /// writes aim at `Mooring::home`.
    pub(crate) scope: Scope,
    pub(crate) session: SessionState,
    pub(crate) local: LocalState,
}

impl Shell {
    /// `poll`: the step boundary's operation.  Raises the stop in force on
    /// the current scope, named for its owner.  `pub`: a host's own
    /// builtins poll it between their steps.
    ///
    /// # Errors
    /// The stop in force, as the escape it is.
    pub fn poll(&self) -> Settled<()> {
        self.scope.poll()
    }

    /// Enter a cleanup: the current scope becomes a shielded child for
    /// `grace` from the strike; the previous one comes back for the frame
    /// to restore.
    pub(crate) fn shield(&mut self, grace: Duration) -> Scope {
        let shielded = self.scope.shielded(grace);
        std::mem::replace(&mut self.scope, shielded)
    }

    /// A call's span becomes the dispatch site, unless it lies outside the
    /// session's sources — the baked prelude's — so a prelude function's
    /// inner command is stamped where the user called the function.
    pub(crate) fn stamp_call_site(&mut self, span: Option<Span>) {
        if let Some(span) = span
            && self.session.sources.get(span.file).is_some()
        {
            self.local.audit.call_site = Some(span);
        }
    }

    /// Run `f` with `span` as the dispatch site, then restore the old one:
    /// what `f` observes is stamped by the syntax that asked for it, not by
    /// whatever a body dispatched last.
    pub(crate) fn at_site<R>(&mut self, span: Option<Span>, f: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.local.audit.call_site;
        self.stamp_call_site(span);
        let r = f(self);
        self.local.audit.call_site = saved;
        r
    }

    /// The dispatch register [`Audit`] carries, resolved against the session's
    /// sources.  Returned by value so the caller may hold `&mut audit`
    /// alongside it.
    /// `pub`, not `pub(crate)`: a host entry that builds its own
    /// [`crate::types::Observation`] (a grep walk, a read outside any
    /// redirect) needs the same call site core's own entries stamp.
    pub fn call_site(&self) -> Option<CallSite> {
        self.session.sources.site(self.local.audit.call_site?)
    }

    /// Put one enquiry to `mooring`'s host desk and block for the answer.  The
    /// absent-desk error is the honest answer of a host that answers none.
    ///
    /// # Errors
    /// Returns `Err` if no desk is installed on this run, or if the installed
    /// `EnquiryDesk` returns one.
    pub fn enquire(
        &self,
        mooring: &Mooring,
        req: crate::first_order::FOValue,
    ) -> Settled<crate::first_order::FOValue> {
        match mooring.desk.as_ref() {
            Some(desk) => desk.enquire(req, &self.scope),
            None => Err(Break::Error(
                Error::raised(crate::types::NO_DESK, crate::types::NO_DESK_STATUS)
                    .with_hint(crate::types::NO_DESK_HINT),
            )),
        }
    }

    /// Register `text`, read from `origin`, returning the [`FileId`] the
    /// compiled program's spans must carry to resolve back to it.
    pub(crate) fn install_script_context(&mut self, origin: &SourceOrigin, text: &str) -> FileId {
        self.session
            .sources
            .register(Source::from_text(origin.clone(), text))
    }

    /// Register a top-level run's script and point [`SessionState::root_file`]
    /// at it.  [`crate::run::run_framed`] alone calls this; a module load takes
    /// [`Self::install_script_context`], which appends without disturbing
    /// `root_file`, so the run's own root keeps its origin.
    pub(crate) fn install_root_context(&mut self, origin: &SourceOrigin, text: &str) -> FileId {
        let file = self.install_script_context(origin, text);
        self.session.root_file = Some(file);
        file
    }

    /// Write `bytes` to the current stdout sink.
    ///
    /// # Errors
    /// See [`Self::write_sink`].
    pub(crate) fn write_stdout(&mut self, bytes: &[u8]) -> Settled<()> {
        Self::write_sink(&mut self.io.stdout, &self.scope, bytes, "stdout")
    }

    /// Write `bytes` to the current stderr sink — where `warn` and the shell's
    /// own diagnostics land, and what `2> f` rebinds.
    ///
    /// # Errors
    /// See [`Self::write_sink`].
    pub fn write_stderr(&mut self, bytes: &[u8]) -> Settled<()> {
        Self::write_sink(&mut self.io.stderr, &self.scope, bytes, "stderr")
    }

    /// The one place a reader leaving becomes the stop it is.  A broken pipe
    /// is the host's reader gone, unnamed, the top's; on stderr alone it is
    /// dropped, as every Unix program drops it.
    pub(crate) fn write_sink(
        sink: &mut crate::io::Sink,
        scope: &Scope,
        bytes: &[u8],
        what: &str,
    ) -> Settled<()> {
        match sink.write_under(scope, bytes) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                if sink.is_stderr() {
                    Ok(())
                } else {
                    Err(Break::stopped(CancelCause::ReaderGone, None))
                }
            }
            Err(e) => Err(match e.downcast::<WriteStopped>() {
                Ok(stop) => Break::stopped(stop.cause, stop.owner),
                Err(e) => sig(format!("could not write to {what}: {e}")),
            }),
        }
    }

    /// The closed session scheme for a value binding — `None` for anything
    /// but a lambda- or block-shaped thunk. Shared by [`Self::bind_value`]
    /// and [`Self::register_hook`], which differ only in what they do with it.
    pub(crate) fn value_scheme(
        &self,
        value: &crate::types::Value,
    ) -> Option<Arc<crate::ty::Scheme>> {
        let (param, body) = match value {
            crate::types::Value::Thunk(closure) => match closure.comp().arrow() {
                Some((param, body)) => (Some(param), body),
                None => (None, closure.comp()),
            },
            _ => return None,
        };
        Some(Arc::new(crate::typecheck::binding_value_scheme(
            param,
            body,
            self.session_schemes(),
        )))
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
