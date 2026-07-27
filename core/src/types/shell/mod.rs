//! Interpreter state.
//!
//! [`Shell`] is the central interpreter state, partitioned by lifetime —
//! the field name *is* the invariant:
//!
//! - [`Mobile`] — the persistable bundle (scope + control + context) that
//!   crosses evaluation boundaries and thread spawns.
//! - `io` ([`Io`]) — the run's byte streams: stdin/stdout/stderr, the
//!   terminal snapshot, the launch role.  A run swaps a fresh set in and
//!   restores the previous one on teardown ([`crate::run::IoLoan`]).
//!   Everything a run fixes *once* — where its events go, who answers it,
//!   what stops it, its terminal authority — is not here at all: it is the
//!   [`Mooring`], which lives on the run's Rust stack frame and reaches
//!   callees as `&Mooring`, disjoint from `&mut Shell`.  Borrow when you
//!   can, loan when you must: run-invariant state threads as `&Mooring` and
//!   the stack restores it; state that genuinely mutates is taken on loan
//!   and repaid.
//! - [`SessionState`] — state that survives every run (durable cancel root,
//!   the anchor top-level runs nest under, source registry, exit hints,
//!   builtin table).
//! - [`LocalState`] — host-local scratch with its own flow rules (audit, REPL).
//!
//! The methods on [`Shell`] live in submodules grouped by concern:
//!
//! - [`init`] — construction and the startup env-var seeding pass.
//! - [`context`] — the [`Context`] impl: dynamic-context verbs.
//! - [`scope`] — `with_*` scope guards (`within` / `grant`), alias
//!   frames, audit-subtree combinators.
//! - [`checks`] — capability-check forwarders to the
//!   `capability::check_*(&Context, …)` decisions.
//! - [`cwd`] — logical working directory and path resolution.
//! - [`hooks`] — the hook table's types and the session's registration
//!   surface ([`Shell::register_hook`] and its inverses).
//! - [`inherit`] — state transfer between parent and child Shells
//!   ([`Shell::with_thunk_body`] for same-thread bodies;
//!   [`Shell::spawn_thread`], [`Shell::child_of`], [`Shell::child_from`]
//!   for forks, plus [`Shell::inherit_from`] / [`Shell::return_to`]).
//!
//! The small primitives that don't fit a concern — error
//! construction, stdout writes, status writes, `$ENV` / `$ARGS`
//! resolution, closure-capture snapshot — live directly on this
//! module.

pub(crate) mod bindings;
mod checks;
mod context;
pub(crate) mod control;
pub(crate) mod cwd;
pub(crate) mod detached;
pub(crate) mod hooks;
mod host;
mod inherit;
mod init;
pub(crate) mod modules;
pub(crate) mod repl;
mod scope;
pub(crate) mod workers;

pub(crate) use inherit::ThunkBody;

use self::bindings::BindingLedger;
use self::control::ControlState;
use self::cwd::Cwd;
use self::detached::DetachPolicy;
use self::modules::Modules;
use self::repl::ReplScratch;
use self::workers::WorkerRegistry;
use super::audit::Audit;
use super::builtin::BuiltinTable;
use super::capability::GrantStack;
use super::env::Env;
use super::env::EnvVars;
use super::error::Error;
use super::handler::HandlerStack;
use super::mooring::{Mooring, NurseryId, SurfaceSink, TerminalAccess};
use super::value::Value;
use crate::diagnostic::CallSite;
use crate::io::Io;
use crate::process::{DurableRoot, ForegroundScope};
use crate::source::{FileId, Source, SourceDb, Span};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

/// Default cap on non-tail closure-call depth.
///
/// Insurance against
/// stack-guard SIGABRT from runaway recursion the typechecker can't
/// catch.  Tail calls are landed in the trampoline loop and don't
/// count.  Overridable via rc / CLI; in practice never tuned.
pub const DEFAULT_RECURSION_LIMIT: usize = 1024;

/// The Send+Clone dynamic context carried by every child computation — a
/// thunk body, a spawned thread (`spawn`, `par`, pipeline stage), or a REPL
/// aside.
///
/// Lives as the `context` field of [`Mobile`] and clones wholesale
/// into a child; the call site and builtin table are deliberately *not*
/// here (they are run- and session-state respectively), so a `Context` clone
/// carries no render registry and no host dispatch.
///
/// Deliberately excluded (each handled separately):
/// - `scope` (sibling on [`Mobile`]): closure-captured per child; not
///   bulk-inherited from parent.
/// - `control` (sibling on [`Mobile`]): per-field flow rules in
///   [`ControlState::inherit_from`]; `last_status` reset on a spawned thread.
/// - the run's own frame ([`Mooring`], `shell.io`): installed afresh per
///   run, flowed per their own manifest, never clone-shared blind.
#[derive(Debug, Clone, Default)]
pub struct Context {
    // ── attenuable by within / grant ─────────────────────────────────────
    /// Process env-var overrides set by `within [shell: …]`.
    /// `pub(crate)` so two privileged callers — the
    /// [`Shell::with_env`] restore step and the child-eval mobile
    /// reconstruction
    /// ([`WireMobile::into_runtime`](crate::subprocess::WireMobile::into_runtime)) —
    /// can install a vetted whole map.  Per-key mutation
    /// goes through [`Context::set_env_var`] (and friends).  `PWD` /
    /// `OLDPWD` are excluded: those keys live on `context.cwd`, and a
    /// copy here would shadow the canonical pair and drift on the next
    /// `cd`.
    pub(crate) env_overrides: EnvVars,
    /// Working directory override set by `within [dir: …]`.  Rolled
    /// back at scope exit; distinct from [`Cwd::current`] (the
    /// `cd`-mutated persistent cwd).
    pub dir: Option<PathBuf>,
    /// Capability restriction stack — innermost last.
    pub grants: GrantStack,
    /// `within [handlers: …, handler: …]` effect-handler stack —
    /// innermost last.
    pub handlers: HandlerStack,

    /// Hook dispatch table — session-lived named run-entry
    /// points (rc-declared prompt, startup block, plugin hooks, …).
    /// A separate namespace from both the user lexical scope and the
    /// handler stack: hooks are run roots, never commands or
    /// readable variables.
    pub hooks: std::collections::HashMap<hooks::HookName, hooks::Hook>,

    // ── dynamic context, not attenuable ─────────────────────────────────
    /// Invocation positional args (`$ARGS`, `$1`, …) passed on the
    /// command line or by `source`.  Inherits with caller; not
    /// modified by `within` / `grant`.
    pub args: Vec<String>,
    /// Module-loader state (stack, depth).
    pub modules: Modules,
    /// Shell-owned logical cwd pair (`cd`-mutated current + `OLDPWD`
    /// companion).  Snapshotted so spawned threads see the current
    /// logical cwd at the spawn point; flowed back on same-thread
    /// thunk return so a `cd` inside a thunk persists.
    pub cwd: Cwd,
}

// Capability checks operate on this dynamic-context cluster — the
// type system thereby prevents policy code from reading lexical scope,
// REPL scratch, control state, or exit hints.  The decisions themselves
// are the `capability::check_*(&Context, …)` functions, each of which
// borrows this Context and folds the whole stack from there.

/// The persistable half of a [`Shell`]: lexical scope, control
/// counters, dynamic [`Context`].
///
/// Cloned on every
/// [`Shell::inherit_from`] / [`Shell::spawn_thread`] and snapshotted
/// across evaluation boundaries via [`Shell::mobile`] /
/// [`Shell::install_mobile`].
#[derive(Clone)]
pub struct Mobile {
    /// Lexical scope chain.  Closure-captured; doesn't flow through
    /// `inherit_from` / `spawn_thread`.  See `types/env.rs`.
    pub scope: Env,
    /// Evaluator control-flow counters: `last_status`, `call_depth`,
    /// `recursion_limit`.  Different
    /// fields obey different flow rules — see
    /// [`Shell::inherit_from`] / [`Shell::return_to`] and
    /// `types/shell/control.rs`.
    pub control: ControlState,
    /// The clone-into-child dynamic context: env/cwd/grants, handlers,
    /// args, and module-loader state.  No source cursor, no builtin table.
    /// See [`Context`] and [`Cwd`] for the cwd-pair semantics.
    pub context: Context,
}

/// State that outlives every run's teardown.
pub struct SessionState {
    /// The session's durable cancel root.  Detached workers (`spawn`,
    /// `watch`, `par`) parent under this rather than under the swappable
    /// foreground [`Mooring::cancel`], so a foreground cancel never reaches
    /// them.  Only a [`RootAbort`](crate::process::CancelCause::RootAbort) on
    /// the root, or a cancel on the worker's own scope, stops such a worker.
    /// Minted deaf to the ambient causes; [`Shell::face_signals`] re-mints
    /// it facing, for the one host that owns the process's signals.  Shared,
    /// not re-minted, by an aside ([`Shell::join_session`]).
    pub(crate) root: DurableRoot,
    /// The scope a *top-level* run nests its foreground frame under — the
    /// role the boot frame's `run.cancel` played while the frame lived on
    /// the `Shell`.  Re-minted wherever the root is
    /// ([`Shell::new`], [`Shell::face_signals`], [`Shell::join_session`]),
    /// and never afterwards: a run entered through
    /// [`Shell::run`](crate::Shell::run) hangs off it, while one entered
    /// through [`Shell::run_nested`](crate::Shell::run_nested) hangs off the
    /// mooring of the run it nests in, so the tree is the LIFO extent it
    /// claims to be.
    pub(crate) anchor: ForegroundScope,
    /// Durable source registry, keyed by [`FileId`](crate::source::FileId).
    /// A run registers its root source and seeds [`Self::root_file`] at run
    /// start, module loads append to it, and hosts read it after the run
    /// returns to render runtime errors. Append-only for the whole session,
    /// so a nested run's spans can never alias an outer run's [`FileId`].
    /// Skipped by IPC/serde — it is render state, not mobile.
    pub(crate) sources: SourceDb,
    /// The current run's root source, as registered by
    /// [`Shell::install_root_context`]. [`FileId::DUMMY`] between runs, and
    /// in the cross-process pipeline-stage child (`crate::child_eval`),
    /// which never roots a run of its own: its `source`/`use` fallback
    /// therefore resolves through `sources.get(FileId::DUMMY)` → `None` →
    /// `""`, i.e. cwd-relative. Saved and restored across a run install
    /// beside `io` and `local.audit.call_site`.
    pub(crate) root_file: FileId,
    /// Exit-code hint table — loaded once at startup from the data directory.
    pub(crate) exit_hints: crate::exit_hints::ExitHints,
    /// Host-installed command dispatch table.  Builtin bodies are Rust fn
    /// pointers or captured host closures — process-local dispatch state, not
    /// serialised ral values, so the receiver of a wire mobile supplies its
    /// own rather than shipping it in a [`WireMobile`](crate::subprocess::WireMobile).
    pub(crate) builtins: BuiltinTable,
    /// Host-installed `name -> doc` entries for a sourced closure library
    /// (exarch's agent helpers) that `help`/`explain` cannot otherwise see —
    /// see [`Shell::install_library_docs`](super::Shell::install_library_docs).
    pub(crate) library_docs: std::collections::HashMap<String, String>,
    /// The session's terminal-foreground witness, minted once at construction
    /// from the startup `tcgetpgrp == getpgrp` predicate. `Some` when ral owns
    /// the controlling terminal's foreground (interactive REPL, terminal-
    /// launched script), `None` otherwise (piped/backgrounded/tty-less, and
    /// every non-Unix platform). Lent — never moved or cloned — to the
    /// foreground handoff via [`Shell::terminal_lease`], and only when the
    /// installed run's [`TerminalAccess`] permits.
    pub(crate) terminal_lease: Option<crate::process::TerminalLease>,
    /// The guest process jail, installed once by
    /// [`crate::engine::run_engine`] when `RAL_GUEST` is set — never on an
    /// ordinary host run. Shared by `Arc`, never cloned-and-reset, across
    /// every fork and spawned worker, so concurrent spawns from sibling
    /// Shells still mint distinct uids and cgroups off the one counter.
    pub(crate) guest_jail: Option<std::sync::Arc<crate::process::jail::GuestJail>>,
}

/// Host-local scratch whose members carry their own flow rules — not a
/// lifetime category, the residue left once run and session state are named.
pub struct LocalState {
    /// Audit collector: the in-flight execution tree plus the current
    /// byte-capture policy.  Scope-introducing builtins (`grant`, `within`,
    /// `guard`, `try`, `audit`) own the children their bodies produce; the
    /// dispatcher posts one node per command via the command lifecycle
    /// in [`crate::evaluator::audit`].
    pub(crate) audit: Audit,
    /// REPL-only scratch state (editor plugin context + queued chpwd
    /// notification).  Doesn't flow across threads or IPC; moved on
    /// cross-process pipeline-stage boundary.  See `types/shell/repl.rs`.
    pub(crate) repl: ReplScratch,
    /// Directory of every worker (`spawn`, `watch`, `service`) detached
    /// from this shell, keyed by nothing but the handle it registered with — see
    /// `types/shell/workers.rs`.  One registry per `Shell`, i.e. one per
    /// agent; [`Shell::spawn_thread`] shares it into a spawned worker's own
    /// shell (so a nested `spawn` registers alongside its parent), but a
    /// sub-agent fork starts with a fresh one.
    pub(crate) workers: WorkerRegistry,
    /// The binding-lease ledger (`types/shell/bindings.rs`,
    /// `decisions/260629_agent-binding-reaping`): inert until a host arms
    /// it, then tracks the idle-call age of every non-baseline top-level
    /// name so an agent host can prune scratch that has gone unused for too
    /// long. Unlike [`Self::workers`] this needs no lock — it has exactly
    /// one writer, the thread that owns `&mut Shell` for every run, install,
    /// and prune (verified in `bindings.rs`'s module doc). One ledger per
    /// `Shell`, i.e. one per agent; a sub-agent fork or spawned worker starts
    /// with a fresh, inert one — nothing shares it, nothing flows back.
    pub(crate) bindings: BindingLedger,
    /// This session's authority to birth processes that outlive it
    /// (`types/shell/detached.rs`): `None` until a host arms it, and armed in
    /// the same act that installs the `detach` builtin, so an unarmed shell
    /// lacks the verb rather than owning one it cannot budget. `Arc`-shared
    /// into a spawned worker's shell like [`Self::workers`], so a `detach`
    /// nested in a `spawn { }` body spends the owning session's budget.
    pub(crate) detach: Option<Arc<DetachPolicy>>,
    /// Whether this state owns its worker registry. True everywhere but a
    /// [`Shell::spawn_thread`] child, which shares its *parent's* registry
    /// by `Arc` clone — a worker's own shell dropping must not cancel its
    /// parent's whole roster. Read by the [`Drop`] below: a session's
    /// workers die when the session's shell is torn down, the ownership
    /// edge the session-ledger ADR calls for, closed once here rather than
    /// at every call site that can end a session's life.
    pub(crate) workers_owned: bool,
}

impl Default for LocalState {
    fn default() -> Self {
        Self {
            audit: Audit::default(),
            repl: ReplScratch::default(),
            workers: WorkerRegistry::default(),
            bindings: BindingLedger::default(),
            detach: None,
            workers_owned: true,
        }
    }
}

/// A session's workers die with the session's shell: cancelling on drop
/// covers every teardown path — an agent's ordinary end, a `/clear`'s shell
/// replacement, a wire session's detach — without a host call site.
impl Drop for LocalState {
    fn drop(&mut self) {
        if self.workers_owned {
            self.workers.cancel_all();
        }
    }
}

/// The runtime, partitioned by lifetime.
///
/// A field either changes within a run
/// (`io`), survives a run ([`SessionState`]), crosses evaluation
/// boundaries ([`Mobile`]), or stays as host scratch ([`LocalState`]).  What
/// a run fixes once and never changes is not here at all: it is the
/// [`Mooring`] the run door owns on its stack.
///
/// Every field is `pub(crate)`: the partition that encodes run safety,
/// capability attenuation, and mobile framing is core's invariant, not a
/// public API a host can reach past.  Hosts drive a session through the
/// intent verbs — the [`mod@host`] accessors and the scope/context verbs —
/// each of which is a complete operation rather than a raw field poke.
/// Durability is core's own: the run door ([`Shell::run`])
/// checkpoints and rolls back the [`Mobile`] around every run.
pub struct Shell {
    pub(crate) mobile: Mobile,
    pub(crate) io: Io,
    pub(crate) session: SessionState,
    pub(crate) local: LocalState,
}

impl Shell {
    /// Construct an unspanned [`Error`]: the break path stamps the span of
    /// the innermost node it unwinds through.  A caller already holding a
    /// better span attaches it with [`Error::at_span`].
    pub fn err(&self, msg: impl Into<String>, status: i32) -> Error {
        Error::new(msg, status)
    }

    /// Like [`Self::err`], with an additional hint.
    pub fn err_hint(&self, msg: impl Into<String>, hint: impl Into<String>, status: i32) -> Error {
        Error::new(msg, status).with_hint(hint)
    }

    /// Resolve `span` to the value-typed [`CallSite`] audit nodes and
    /// capability checks carry: the name of its source in this session's
    /// registry plus the 1-indexed line and column of its start.
    /// [`CallSite::default`] when there is no span, or its source is not
    /// registered here.
    pub(crate) fn site_of(&self, span: Option<Span>) -> CallSite {
        let resolved = span.and_then(|s| self.session.sources.get(s.file).map(|src| (s, src)));
        let Some((span, source)) = resolved else {
            return CallSite::default();
        };
        let (line, col) = source.byte_to_line_col(span.start as usize);
        CallSite {
            script: source.name().to_string(),
            line,
            col,
        }
    }

    /// This run's call site as a [`CallSite`] — [`Self::site_of`] applied to
    /// the dispatch register [`Audit`] carries (`local.audit.call_site`).
    /// Taken by value so the caller may hold `&mut audit` alongside it.
    pub(crate) fn call_site(&self) -> CallSite {
        self.site_of(self.local.audit.call_site)
    }

    /// Put one enquiry to `mooring`'s host desk and block for the answer.
    /// The absent-desk error is the honest answer of a host that answers none
    /// (the bare REPL, and any host before the migration installs its desk).
    ///
    /// # Errors
    /// Returns `Err` if no desk is installed on this run, or if the
    /// installed desk's [`EnquiryDesk::enquire`] returns one.
    pub fn enquire(
        &self,
        mooring: &Mooring,
        req: crate::serial::FOValue,
    ) -> Result<crate::serial::FOValue, crate::types::Error> {
        match mooring.desk.as_ref() {
            Some(desk) => desk.enquire(req, mooring.cancel.as_scope()),
            None => Err(self.err("this host answers no enquiries", 1)),
        }
    }

    /// Fork this shell ([`Self::fork_session`]) and park the fork in
    /// `mooring`'s nursery, returning the [`NurseryId`] a desk handler — barred
    /// by the reentrancy law from holding `&mut Shell` itself — later
    /// redeems with [`Nursery::adopt`]. The absent-nursery error is the
    /// honest answer of a host that adopts no forked sessions (the bare
    /// REPL, and any host before it installs a nursery).
    ///
    /// # Errors
    /// Returns `Err` if no nursery is installed on this run.
    pub fn fork_into_nursery(&self, mooring: &Mooring) -> crate::types::Settled<NurseryId> {
        match mooring.nursery.as_ref() {
            Some(nursery) => Ok(nursery.park(self.fork_session())),
            None => Err(crate::types::Break::Error(
                self.err("this host adopts no forked sessions", 1),
            )),
        }
    }

    /// Atomically overwrite `path` with `bytes` through core's full
    /// `>`-redirect write recipe — symlink-resolved, mode-preserving,
    /// fsync-durable — while emitting no io event.  The public write door for a
    /// host builtin (exarch's `edit-hash`/`edit-replace`) that must write *below* the
    /// redirect frame and speak its own surface: it shares the one atomic
    /// recipe instead of forking a weaker temp-file write that silently narrows
    /// the target's mode, follows symlinks by replacing them, and skips the
    /// durability flush.
    ///
    /// # Errors
    /// Returns `Err` if the target cannot be opened for writing, if the
    /// write fails, or if the atomic commit (temp-file rename and fsync)
    /// fails.
    pub fn atomic_write(&mut self, path: &str, bytes: &[u8]) -> crate::types::Settled<()> {
        crate::runtime::command::atomic_write(path, bytes, self)
    }

    /// Install a script context: register `text` under display `name` in the
    /// session registry, returning the [`FileId`] the compiled program's
    /// spans must carry so they resolve to it.
    pub fn install_script_context(&mut self, name: &str, text: &str) -> FileId {
        self.session.sources.register(Source::from_text(name, text))
    }

    /// Install a top-level run's script context: register it and set
    /// [`SessionState::root_file`] to the id it landed at.  Used only at the
    /// top-level run boundary ([`crate::run::run_framed`]); module loads
    /// stay on [`install_script_context`](Self::install_script_context),
    /// which appends without touching `root_file`, so a run's `source`d
    /// modules remain resolvable — and the run's own root stays named — when
    /// its error renders at end of run.  The registry is append-only, so a
    /// nested run ([`Shell::run_nested`](crate::Shell::run_nested)) mints a
    /// fresh [`FileId`] here rather than re-minting one the outer run's
    /// spans still carry.
    pub fn install_root_context(&mut self, name: &str, text: &str) -> FileId {
        let file = self.install_script_context(name, text);
        self.session.root_file = file;
        file
    }

    /// Install a script context under an already-minted [`FileId`] — the
    /// seam a re-exec'd pipeline-stage child uses to resolve its spans
    /// against the exact source and file identity its parent already
    /// registered, rather than minting a second, differently-numbered copy
    /// in its own (empty) session registry.
    pub(crate) fn install_remote_context(&mut self, name: &str, file: FileId, text: &str) {
        self.session
            .sources
            .register_at(file, Source::from_text(name, text));
    }

    /// Resolve the six pseudo-variables (`$ENV`, `$ARGS`, `$NPROC`, `$CWD`,
    /// `$STATUS`, `$USER`).  All-caps names are the shell's; lowercase names
    /// are the user's.  `$SCRIPT` is not among them: it is baked to a string
    /// literal by the elaborator from the file it is compiling, lexical by
    /// construction, so no runtime reader exists for it.  These are computed
    /// on demand rather than stored in scope, so [`Self::lookup_value_name`]
    /// consults them after lexical scope and before the builtin table.  Any
    /// other name returns `None`.
    pub fn pseudo_var(&self, name: &str) -> Option<Value> {
        match name {
            "ENV" => {
                // PWD / OLDPWD are shell-cwd-derived, not
                // env-overrides: they live on `context.cwd` and
                // change every `cd`.  Drop them at the source so
                // `$ENV` reads as the rule.
                let mut merged: HashMap<String, String> = std::env::vars()
                    .filter(|(k, _)| !matches!(k.as_str(), "PWD" | "OLDPWD"))
                    .collect();
                merged.extend(
                    self.mobile
                        .context
                        .env_overrides
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone())),
                );
                let pairs: Vec<_> = merged
                    .into_iter()
                    .map(|(k, v)| (k, Value::String(v)))
                    .collect();
                Some(Value::map(pairs))
            }
            "ARGS" => Some(Value::list(
                self.mobile
                    .context
                    .args
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            )),
            "NPROC" => Some(Value::Int(std::thread::available_parallelism().map_or(
                1,
                |n| {
                    #[allow(
                        clippy::cast_possible_wrap,
                        reason = "CPU count from available_parallelism is tiny"
                    )]
                    {
                        n.get() as i64
                    }
                },
            ))),
            "CWD" => {
                let p = self.cwd();
                let home = crate::path::home_from_env();
                let cwd_str = crate::path::abbreviate_home(&p, &home);
                let cwd_str = if cwd_str.is_empty() {
                    "?".into()
                } else {
                    cwd_str
                };
                Some(Value::String(cwd_str))
            }
            "STATUS" => Some(Value::Int(i64::from(self.mobile.control.last_status))),
            "USER" => Some(Value::String(crate::path::user_name_from_env())),
            _ => None,
        }
    }

    /// Resolve `name` at value position (`$name` and other
    /// [`crate::ir::Val::Variable`] uses). Value lookup is binding-only:
    /// lexical scope, pseudo variables, and explicitly reified
    /// host/builtin bindings. User aliases and `within` handlers are
    /// operation handlers, not first-class values.
    pub fn lookup_value_name(&self, name: &str) -> Option<Value> {
        if let Some(v) = self.mobile.scope.get(name) {
            return Some(v.clone());
        }
        if let Some(v) = self.pseudo_var(name) {
            return Some(v);
        }
        let entry = self.session.builtins.get(name)?;
        crate::builtins::synthesize_builtin_value(&entry)
    }

    /// Set `last_status` from a boolean (`true` → 0, `false` → 1).
    #[inline]
    pub fn set_status_from_bool(&mut self, ok: bool) {
        self.mobile.control.last_status = i32::from(!ok);
    }

    /// Write `bytes` to the current stdout sink.
    ///
    /// `BrokenPipe` is treated as a clean shutdown: the downstream
    /// reader has closed its end of the pipe (e.g. `fzf` accepted a
    /// selection, `head` took its quota), so further writes are
    /// pointless but not an error.  This matches traditional Unix
    /// tools, which exit silently on `SIGPIPE`, and prevents the
    /// pipeline supervisor from interpreting an EPIPE on a builtin
    /// writer as a failure that warrants tearing the pgid down with
    /// `SIGKILL` — a teardown that would surface as exit status 137
    /// on sibling stages that had themselves exited cleanly.
    ///
    /// # Errors
    /// Returns `Err` if the underlying write fails with anything other than
    /// `BrokenPipe`, which is swallowed as a clean shutdown.
    pub fn write_stdout(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self.io.stdout.write_all(bytes) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Snapshot the current scope chain for closure capture.  Returns
    /// an `Arc<Env>` so multiple closures (e.g. a `letrec` bank)
    /// created from one snapshot share one allocation; subsequent
    /// thunk clones are refcount bumps.
    pub fn snapshot(&self) -> Arc<Env> {
        Arc::new(self.mobile.scope.clone())
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new(crate::io::TerminalState::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Nursery;
    use std::sync::Mutex;

    #[test]
    fn pseudo_var_cwd_is_live() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let val = shell.pseudo_var("CWD").expect("$CWD must resolve");
        match val {
            Value::String(s) => assert!(!s.is_empty(), "$CWD must be non-empty"),
            other => panic!("$CWD must be a String, got {other:?}"),
        }
    }

    #[test]
    fn pseudo_var_status_reflects_last_status() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.set_status_from_bool(false);
        let val = shell.pseudo_var("STATUS").expect("$STATUS must resolve");
        assert_eq!(val, Value::Int(1));
        shell.set_status_from_bool(true);
        let val = shell.pseudo_var("STATUS").expect("$STATUS must resolve");
        assert_eq!(val, Value::Int(0));
    }

    #[test]
    fn pseudo_var_user_is_live() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let val = shell.pseudo_var("USER").expect("$USER must resolve");
        match val {
            Value::String(s) => assert!(!s.is_empty(), "$USER must be non-empty"),
            other => panic!("$USER must be a String, got {other:?}"),
        }
    }

    #[test]
    fn pseudo_var_unknown_returns_none() {
        let shell = Shell::new(crate::io::TerminalState::default());
        assert!(shell.pseudo_var("NOSUCHVAR").is_none());
    }

    #[test]
    fn lookup_value_name_sees_pseudo_vars() {
        let shell = Shell::new(crate::io::TerminalState::default());
        assert!(shell.lookup_value_name("CWD").is_some());
        assert!(shell.lookup_value_name("STATUS").is_some());
        assert!(shell.lookup_value_name("USER").is_some());
    }

    /// The nursery twin of `enquire`'s absent-desk contract: a `Shell` with
    /// no nursery installed answers `fork_into_nursery` with the honest
    /// absence error, verbatim.
    #[test]
    fn fork_into_nursery_errors_honestly_without_a_nursery() {
        let shell = Shell::new(crate::io::TerminalState::default());
        match shell
            .fork_into_nursery(&Mooring::adrift())
            .expect_err("no nursery is installed")
        {
            crate::types::Break::Error(e) => {
                assert_eq!(e.message, "this host adopts no forked sessions");
            }
            other @ crate::types::Break::Escape(_) => {
                panic!("expected Break::Error, got {other:?}")
            }
        }
    }

    /// A nursery installed on the run round-trips a session fork:
    /// `fork_into_nursery` parks a [`Nursery::park`] entry and hands back its
    /// id, and `Nursery::adopt` redeems that id for a live child `Shell` —
    /// one that still carries the parent's whole lexical scope, exactly as
    /// `fork_session` promises (mirrors
    /// `fork_session_holds_no_terminal_authority`'s style in `inherit.rs`).
    #[test]
    fn nursery_round_trips_a_forked_session() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell
            .mobile
            .scope
            .set("parent_binding".to_string(), Value::Int(42));
        let nursery = Nursery::default();
        let mooring = Mooring {
            nursery: Some(nursery.clone()),
            ..Mooring::adrift()
        };

        let id = shell
            .fork_into_nursery(&mooring)
            .expect("a nursery is installed");
        let child = nursery
            .adopt(id)
            .expect("the parked fork must be adoptable");

        assert_eq!(
            child.mobile.scope.get("parent_binding"),
            Some(&Value::Int(42)),
            "the fork must carry the parent's lexical scope"
        );
    }

    /// The run door's own nursery-emptying promise ([`NurseryGuard`], in
    /// `run.rs`) holds even when the run's evaluation panics: a fork parked
    /// during `pre_exec` and never adopted must not survive a run whose body
    /// panics before it settles, because `NurseryGuard`'s `Drop` fires on
    /// unwind exactly as it does on a clean return.
    #[test]
    fn run_door_panic_still_empties_nursery() {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let nursery = Nursery::default();
        let parked_id: Arc<Mutex<Option<NurseryId>>> = Arc::new(Mutex::new(None));

        struct ParkThenPanic(Arc<Mutex<Option<NurseryId>>>);
        impl crate::run::RunLifecycle for ParkThenPanic {
            fn pre_exec(&mut self, mooring: &Mooring, shell: &mut Shell, _src: &str) {
                let id = shell
                    .fork_into_nursery(mooring)
                    .expect("a nursery is installed on this run");
                *self.0.lock().unwrap() = Some(id);
                panic!("run-door test: deliberate panic after parking a fork");
            }
        }

        let _ = shell.run(crate::run::RunRequest {
            run: crate::transport::Run {
                program: crate::transport::Program::Source("$[1 + 1]".into()),
                script_name: "<test>".into(),
                caps: crate::types::Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: crate::run::RunIo::Capture,
                terminal: crate::run::RequestedTerminalAccess::Denied,
                stdin: crate::run::RunStdin::Empty,
            },
            surface: None,
            deferred: None,
            desk: None,
            nursery: Some(nursery.clone()),
            lifecycle: Box::new(ParkThenPanic(parked_id.clone())),
        });

        let id = parked_id
            .lock()
            .unwrap()
            .expect("pre_exec must park a fork before panicking");
        assert!(
            nursery.adopt(id).is_none(),
            "a run-door panic must still empty the nursery on unwind"
        );
    }
}
