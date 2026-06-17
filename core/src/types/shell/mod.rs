//! Interpreter state.
//!
//! [`Shell`] is the central interpreter state, partitioned into four fields
//! by lifetime — the field name *is* the invariant:
//!
//! - [`Mobile`] — the persistable bundle (scope + control + context) that
//!   crosses evaluation boundaries and thread spawns.
//! - [`TurnState`] — the dynamic frame a top-level turn installs (IO, surface
//!   sink, foreground cancel scope, source cursor) and restores on teardown.
//! - [`SessionState`] — state that survives every turn (durable cancel root,
//!   source registry, exit hints, builtin table).
//! - [`LocalState`] — host-local scratch with its own flow rules (audit, REPL).
//!
//! The methods on [`Shell`] live in submodules grouped by concern:
//!
//! - `init` — construction and the startup env-var seeding pass.
//! - `context` — the [`Context`] impl: dynamic-context verbs.
//! - `scope` — `with_*` scope guards (`within` / `grant`), alias
//!   frames, audit-subtree combinators.
//! - `checks` — capability-check forwarders to the
//!   `capability::check_*(&Context, …)` decisions.
//! - `cwd` — logical working directory and path resolution.
//! - `inherit` — state transfer between parent and child Shells
//!   ([`Shell::with_child`], [`Shell::spawn_thread`],
//!   [`Shell::child_from`], [`Shell::with_block`], plus
//!   [`Shell::inherit_from`] / [`Shell::return_to`]).
//!
//! The small primitives that don't fit a concern — error
//! construction, stdout writes, status writes, `$env` / `$args`
//! resolution, closure-capture snapshot — live directly on this
//! module.

mod checks;
mod context;
pub(crate) mod control;
pub(crate) mod cwd;
mod host;
mod inherit;
mod init;
pub(crate) mod modules;
pub(crate) mod repl;
mod scope;

use self::control::ControlState;
use self::cwd::Cwd;
use self::modules::Modules;
use self::repl::ReplScratch;
use super::audit::{Audit, LocationCursor};
use super::capability::GrantStack;
use super::env::Env;
use super::env::EnvVars;
use super::error::Error;
use super::value::{BuiltinTable, HandlerStack, Value};
use crate::diagnostic::{Source, SourceDb};
use crate::io::Io;
use crate::process::{DurableRoot, ForegroundScope};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

/// Default cap on non-tail closure-call depth.  Insurance against
/// stack-guard SIGABRT from runaway recursion the typechecker can't
/// catch.  Tail calls are landed in the trampoline loop and don't
/// count.  Overridable via rc / CLI; in practice never tuned.
pub const DEFAULT_RECURSION_LIMIT: usize = 1024;

/// The Send+Clone dynamic context carried by every child computation — a
/// thunk body, a spawned thread (`spawn`, `par`, pipeline stage), or a REPL
/// aside.  Lives as the `context` field of [`Mobile`] and clones wholesale
/// into a child; the source cursor and builtin table are deliberately *not*
/// here (they are turn- and session-state respectively), so a `Context` clone
/// carries no render registry and no host dispatch.
///
/// Deliberately excluded (each handled separately):
/// - `scope` (sibling on [`Mobile`]): closure-captured per child; not
///   bulk-inherited from parent.
/// - `control` (sibling on [`Mobile`]): per-field flow rules in
///   [`ControlState::inherit_from`]; `last_status` reset on a spawned thread.
/// - the [`TurnState`] substates (`io`, `surface`, `cancel`, `loc`): installed
///   afresh per turn, flowed per their own manifest, never clone-shared blind.
#[derive(Debug, Clone, Default)]
pub struct Context {
    // ── attenuable by within / grant ─────────────────────────────────────
    /// Process env-var overrides set by `within [shell: …]`.
    /// `pub(crate)` so two privileged callers — the
    /// [`Shell::with_env`] restore step and the sandbox IPC ambient
    /// install — can install a vetted whole map.  Per-key mutation
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

    // ── dynamic context, not attenuable ─────────────────────────────────
    /// Invocation positional args (`$args`, `$1`, …) passed on the
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
/// counters, dynamic [`Context`].  Cloned on every
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
    /// `types/control.rs`.
    pub control: ControlState,
    /// The clone-into-child dynamic context: env/cwd/grants, handlers,
    /// args, and module-loader state.  No source cursor, no builtin table.
    /// See [`Context`] and [`Cwd`] for the cwd-pair semantics.
    pub context: Context,
}

/// A sink for structured host events: the value-typed dual of the byte
/// [`Io`](crate::io::Io) sinks.  A host installs one for the extent of an
/// evaluation; the `surface` builtin forwards its argument to it, and is the
/// identity when none is installed.  `Send + Sync` so it propagates into thunk
/// bodies and spawned stages alongside the rest of the child subtree.
pub type SurfaceSink = Arc<dyn Fn(Value) + Send + Sync>;

/// The whole dynamic frame a top-level turn installs.  A turn builds one,
/// swaps it into `shell.turn`, runs, and restores the previous one on
/// teardown; the field is the invariant "the turn-local part" used to be a
/// scattered save/restore.  Same-thread bodies flow these through
/// [`TurnState::inherit_from`] / [`TurnState::return_to`]; a spawned worker
/// builds a fresh one under the durable root.
pub struct TurnState {
    /// Pipeline-stage IO: streams, value channel, terminal state, flags.
    pub(crate) io: Io,
    /// Host-installed sink for structured events surfaced by the `surface`
    /// builtin.  `None` outside a host (e.g. a bare REPL), in which case
    /// `surface` is the identity.  Cloned into thunk bodies and spawned
    /// stages — the `Arc` is shared, never folded back.
    pub(crate) surface: Option<SurfaceSink>,
    /// The turn's foreground work scope.  `signal::check` consults it between
    /// effectful steps; a foreground cancel (turn timeout, Ctrl-C) unwinds
    /// the same-thread work that shares it.  Always a descendant of
    /// [`SessionState::root`] by construction.
    pub(crate) cancel: ForegroundScope,
    /// Turn-local source cursor: every non-registry source-position field.
    /// Read when building a [`SourceLoc`](crate::diagnostic::SourceLoc);
    /// resolved at render time against [`SessionState::sources`].
    pub(crate) loc: LocationCursor,
    /// Lifetime ceiling for workers this turn detaches at the durable
    /// root.  `None` (an interactive host) leaves a worker until `cancel`,
    /// root abort, or session exit; `Some(d)` (an agent host) reaps an
    /// abandoned worker `d` after it is spawned.  Supplied by the frame and
    /// flowed into same-thread bodies and spawned workers so a `spawn`
    /// nested in a thunk sees the same ceiling.
    pub(crate) detached_ceiling: Option<std::time::Duration>,
}

impl TurnState {
    /// Same-thread child flow-in: clone the byte sinks, move the read-once
    /// stdin out of the parent, and share the foreground scope and source
    /// cursor.  Mirror of [`Self::return_to`].
    pub fn inherit_from(&mut self, parent: &mut TurnState) {
        self.io.inherit_from(&mut parent.io);
        self.surface = parent.surface.clone();
        self.cancel = parent.cancel.clone();
        self.loc = parent.loc.clone();
        self.detached_ceiling = parent.detached_ceiling;
    }

    /// Same-thread child flow-out: return the read-once stdin to the parent
    /// so sibling calls see the unconsumed pipe.  The cursor and foreground
    /// do not flow back — the asymmetry is the point.
    pub fn return_to(&mut self, parent: &mut TurnState) {
        self.io.return_to(&mut parent.io);
    }
}

/// State that outlives every turn's teardown.
pub struct SessionState {
    /// The session's durable cancel root.  Detached workers (`spawn`,
    /// `watch`, `par`) parent under this rather than under the swappable
    /// foreground [`TurnState::cancel`], so a foreground cancel never reaches
    /// them.  Only a [`RootAbort`](crate::process::CancelCause::RootAbort) on
    /// the root, or a cancel on the worker's own scope, stops such a worker.
    pub(crate) root: DurableRoot,
    /// Durable source registry, keyed by [`FileId`](crate::source::FileId).
    /// A turn resets and seeds it at turn start, module loads append to it,
    /// and hosts read it after the turn returns to render runtime errors.
    /// Durable across the teardown of [`TurnState::loc`], not across all
    /// future turns.  Skipped by IPC/serde — it is render state, not mobile.
    pub(crate) sources: SourceDb,
    /// Exit-code hint table — loaded once at startup from the data directory.
    pub(crate) exit_hints: crate::exit_hints::ExitHints,
    /// Host-installed command dispatch table.  Builtin bodies are Rust fn
    /// pointers or captured host closures — process-local dispatch state, not
    /// serialised ral values, so the receiver of a wire mobile supplies its
    /// own rather than shipping it in a `WireMobile`.
    pub(crate) builtins: BuiltinTable,
}

/// Host-local scratch whose members carry their own flow rules — not a
/// lifetime category, the residue left once turn and session state are named.
#[derive(Default)]
pub struct LocalState {
    /// Audit collector: the in-flight execution tree plus the current
    /// byte-capture policy.  Scope-introducing builtins (`grant`, `within`,
    /// `guard`, `try`, `audit`) own the children their bodies produce; the
    /// dispatcher posts one node per command via
    /// [`crate::evaluator::audit::finish_command`].
    pub(crate) audit: Audit,
    /// REPL-only scratch state (editor plugin context + queued chpwd
    /// notification).  Doesn't flow across threads or IPC; moved on
    /// same-thread thunk boundary.  See `types/repl.rs`.
    pub(crate) repl: ReplScratch,
}

/// The runtime, partitioned by lifetime.  A field either moves as a turn
/// ([`TurnState`]), survives a turn ([`SessionState`]), crosses evaluation
/// boundaries ([`Mobile`]), or stays as host scratch ([`LocalState`]).
///
/// `mobile` is the public embedding seam; `turn` / `session` / `local` are
/// `pub(crate)` — the fields that encode turn safety are not a public API.
/// Hosts drive a session through the accessors in [`mod@host`].
pub struct Shell {
    pub mobile: Mobile,
    pub(crate) turn: TurnState,
    pub(crate) session: SessionState,
    pub(crate) local: LocalState,
}

impl Shell {
    /// Construct an [`Error`] located at the current source position.
    pub fn err(&self, msg: impl Into<String>, status: i32) -> Error {
        Error::new(msg, status).at_loc(self.turn.loc.source_loc(0))
    }

    /// Like [`Self::err`], with an additional hint.
    pub fn err_hint(&self, msg: impl Into<String>, hint: impl Into<String>, status: i32) -> Error {
        Error::new(msg, status)
            .at_loc(self.turn.loc.source_loc(0))
            .with_hint(hint)
    }

    /// Install a script context: set the active script name on both the
    /// cursor's `script` and `call_site.script`, register the source text in
    /// the session registry, and make the registered source the active one
    /// (`current` + the `source` cache used for byte-span → (line, col)
    /// resolution).  All of these move together — diverging them produced
    /// span-to-position drift inside loaded modules (the parent's `source`
    /// was consulted for the child's spans).
    ///
    /// One-shot install: callers that need restore-on-exit semantics should
    /// use [`crate::builtins::modules::ScriptContextGuard`] instead.
    pub fn install_script_context(&mut self, name: String, text: &str) {
        let source = Source::from_text(&name, text);
        self.turn.loc.current = self.session.sources.register(source.clone());
        self.turn.loc.script = name.clone();
        self.turn.loc.call_site.script = name;
        self.turn.loc.source = Some(source);
    }

    /// Install a top-level turn's script context after clearing the session
    /// registry, so the prior turn's sources are reclaimed before this turn
    /// registers its own.  Used only at the interactive turn boundary (REPL /
    /// shell); module loads stay on
    /// [`install_script_context`](Self::install_script_context), which appends
    /// so a turn's `source`d modules remain resolvable when its error renders
    /// at end of turn.
    pub fn install_root_context(&mut self, name: String, text: &str) {
        self.session.sources.reset();
        self.install_script_context(name, text);
    }

    /// Resolve the four pseudo-variables (`$env`, `$args`,
    /// `$script`, `$nproc`).  These are computed at access time
    /// rather than stored in scope, so they sit between the env
    /// check and the handler-stack walk in [`Self::resolve_value`].
    /// Any other name returns `None`.
    pub fn pseudo_var(&self, name: &str) -> Option<Value> {
        match name {
            "env" => {
                // PWD / OLDPWD are shell-cwd-derived, not
                // env-overrides: they live on `context.cwd` and
                // change every `cd`.  Drop them at the source so
                // `$env` reads as the rule.
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
            "args" => Some(Value::list(
                self.mobile
                    .context
                    .args
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            )),
            // $script: path of the currently-executing file.  Empty
            // in the REPL, under `-c`, and during prelude loading.
            "script" => match self.turn.loc.script.as_str() {
                "" | "-c" | "<prelude>" => None,
                s => Some(Value::String(s.to_string())),
            },
            "nproc" => Some(Value::Int(
                std::thread::available_parallelism()
                    .map(|n| n.get() as i64)
                    .unwrap_or(1),
            )),
            _ => None,
        }
    }

    /// Resolve `name` at value position (`$name` and other
    /// [`Val::Variable`] uses). Value lookup is binding-only:
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
        self.mobile.control.last_status = if ok { 0 } else { 1 };
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
    pub fn write_stdout(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self.turn.io.stdout.write_all(bytes) {
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
        Self::new(Default::default())
    }
}
