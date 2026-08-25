//! Interpreter state, partitioned by lifetime: the field name *is* the
//! invariant.  [`Mobile`] persists and crosses evaluation boundaries and
//! thread spawns; `io` ([`Io`]) belongs to one run, swapped in and restored on
//! teardown by [`crate::run::IoLoan`]; [`SessionState`] survives every run;
//! [`LocalState`] is host scratch whose members each carry their own rule.
//!
//! What a run fixes *once* — where its events go, who answers it, what stops
//! it, its terminal authority — is not here at all: it is the [`Mooring`] on
//! the run's Rust stack frame, reaching callees as `&Mooring` disjoint from
//! `&mut Shell`.  Borrow what a run cannot change, loan what it can.
//!
//! [`Shell`]'s methods live in the submodules below, by concern; the primitives
//! too small to belong to one stay here.

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
use super::mooring::{Fork, Mooring, NurseryId, SurfaceSink, TerminalAccess};
use super::value::Value;
use crate::diagnostic::CallSite;
use crate::io::Io;
use crate::process::{DurableRoot, ForegroundScope};
use crate::source::{FileId, Source, SourceDb, Span};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

/// Default cap on non-tail closure-call depth: insurance against a
/// stack-guard SIGABRT the typechecker cannot catch.  Tail calls land in the
/// trampoline loop and do not count.
pub const DEFAULT_RECURSION_LIMIT: usize = 1024;

/// The Send+Clone dynamic context every child computation carries — a thunk
/// body, a spawned thread (`spawn`, `par`, pipeline stage), a REPL aside.
///
/// It clones wholesale into a child, so what is *not* here is the point: its
/// [`Mobile`] siblings each flow by their own rule, the run's own frame is
/// installed afresh, and call site and builtin table stay run and session
/// state — so a clone carries no render registry and no host dispatch.
#[derive(Debug, Clone, Default)]
pub struct Context {
    // ── attenuable by within / grant ─────────────────────────────────────
    /// Process env-var overrides set by `within [env: …]`.  `PWD` / `OLDPWD`
    /// are excluded: those keys live on `cwd` below, and a copy here would
    /// shadow the canonical pair and drift on the next `cd`.
    pub(crate) env_overrides: EnvVars,
    /// Working directory override set by `within [dir: …]`, rolled back at
    /// scope exit — distinct from [`Cwd::current`], the `cd`-mutated one.
    dir: Option<PathBuf>,
    /// Capability restrictions, innermost last.
    pub grants: GrantStack,
    /// `within [handlers: …, handler: …]` effect-handler stack, innermost last.
    pub handlers: HandlerStack,

    /// Session-lived named run entry points (rc-declared prompt, startup block,
    /// plugin hooks) — a namespace apart from both lexical scope and the
    /// handler stack, since hooks are run roots, never commands or variables.
    pub hooks: std::collections::HashMap<hooks::HookName, hooks::Hook>,

    // ── dynamic context, not attenuable ─────────────────────────────────
    /// Invocation positionals (`$ARGS`, `$1`, …), from the command line or `source`.
    pub args: Vec<String>,
    pub modules: Modules,
    /// Snapshotted, so a spawned thread sees the logical cwd as of its spawn
    /// point; flowed back on same-thread thunk return, so a `cd` inside a
    /// thunk persists.
    cwd: Cwd,
}

// The `capability::check_*(&Context, …)` decisions fold the whole stack from
// this one borrow, so the type system bars policy code from ever reading
// lexical scope, REPL scratch, control state, or exit hints.

/// The persistable half of a [`Shell`]: cloned on every
/// [`Shell::inherit_from`] / [`Shell::spawn_thread`], snapshotted across
/// evaluation boundaries by [`Shell::mobile`] / [`Shell::install_mobile`].
#[derive(Clone)]
pub struct Mobile {
    /// Closure-captured, so it does *not* flow through `inherit_from` or
    /// `spawn_thread` the way the rest of `Mobile` does.
    pub scope: Env,
    /// `last_status`, `call_depth`, `recursion_limit` — each with its own flow
    /// rule, spelled out in [`ControlState::inherit_from`].
    pub control: ControlState,
    pub context: Context,
}

/// State that outlives every run's teardown.
pub struct SessionState {
    /// Detached workers (`spawn`, `watch`, `par`) parent here, not under the
    /// run's swappable foreground scope, so a foreground cancel never reaches
    /// them.  Minted deaf to the ambient causes; [`Shell::face_signals`]
    /// re-mints it facing, for the one host that owns the process's signals.
    pub(crate) root: DurableRoot,
    /// The scope a *top-level* run nests its foreground frame under.  A run
    /// entered through [`Shell::run`](crate::Shell::run) hangs off this, while
    /// [`Shell::run_nested`](crate::Shell::run_nested) hangs off the mooring of
    /// the run it nests in, so the tree is the LIFO extent it claims to be.
    pub(crate) anchor: ForegroundScope,
    /// Durable source registry, read by hosts *after* a run returns to render
    /// its runtime errors.  Append-only for the whole session, so a nested
    /// run's spans can never alias an outer run's [`FileId`].
    pub(crate) sources: SourceDb,
    /// The current run's root source.  [`FileId::DUMMY`] between runs, and in
    /// the pipeline-stage child (`crate::child_eval`), which roots no run of
    /// its own: its `source` / `use` fallback therefore resolves through a
    /// missing entry to `""`, i.e. cwd-relative.
    pub(crate) root_file: FileId,
    pub(crate) exit_hints: crate::exit_hints::ExitHints,
    /// Builtin bodies are Rust fn pointers or captured host closures, hence
    /// process-local: the receiver of a wire mobile installs its own table
    /// rather than having one shipped to it.
    pub(crate) builtins: BuiltinTable,
    /// Host-installed `name -> doc` for a sourced closure library (exarch's
    /// agent helpers) that `help` / `explain` cannot otherwise see.
    pub(crate) library_docs: std::collections::HashMap<String, String>,
    /// Minted once from the startup `tcgetpgrp == getpgrp` predicate: `Some`
    /// when ral owns the controlling terminal's foreground, `None` when piped,
    /// backgrounded, tty-less, or off Unix.  Lent — never moved or cloned — to
    /// the handoff, and only when the run's [`TerminalAccess`] permits.
    pub(crate) terminal_lease: Option<crate::process::TerminalLease>,
    /// Installed by [`crate::engine::run_engine`] only when `RAL_GUEST` is set.
    /// Shared by `Arc` across every fork and spawned worker, so concurrent
    /// spawns from sibling Shells mint distinct uids and cgroups off one counter.
    pub(crate) guest_jail: Option<std::sync::Arc<crate::process::jail::GuestJail>>,
}

/// Host-local scratch whose members each carry their own flow rule — not a
/// lifetime category, the residue left once run and session state are named.
pub struct LocalState {
    /// The in-flight audit trail and its byte-capture policy.  `grant`,
    /// `within`, `guard`, `try`, and `audit` are collection boundaries, not
    /// observations; the dispatcher posts one observation per real command
    /// through [`crate::evaluator::audit`].
    pub(crate) audit: Audit,
    /// Flows across neither threads nor IPC; moved across the pipeline-stage
    /// boundary.
    pub(crate) repl: ReplScratch,
    /// Every worker (`spawn`, `watch`, `service`) detached from this shell.
    /// [`Shell::spawn_thread`] shares the registry into the worker's own shell,
    /// so a nested `spawn` registers alongside its parent; a sub-agent fork
    /// starts fresh, one registry per agent.
    pub(crate) workers: WorkerRegistry,
    /// Inert until a host arms it, then ages every non-baseline top-level name
    /// so long-unused scratch can be pruned.  Unlike [`Self::workers`] it needs
    /// no lock: its one writer is the thread owning `&mut Shell` for every run,
    /// install, and prune.  Forks and workers start fresh and inert.
    pub(crate) bindings: BindingLedger,
    /// Authority to birth processes outliving this session.  Armed in the same
    /// act that installs the `detach` builtin, so an unarmed shell lacks the
    /// verb rather than owning one it cannot budget.  `Arc`-shared into a
    /// worker's shell, so a `detach` inside `spawn { }` spends this budget.
    pub(crate) detach: Option<Arc<DetachPolicy>>,
    /// False only in a [`Shell::spawn_thread`] child, which shares its
    /// *parent's* registry by `Arc` clone: a worker's own shell dropping must
    /// not cancel its parent's whole roster.  Read by the [`Drop`] below.
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

/// A session's workers die with the session's shell, external children and
/// their grandchildren included: [`WorkerRegistry::cancel_all`] cancels, then
/// waits for the kills to land.  Dropping covers every teardown path — an
/// agent's ordinary end, a `/clear`'s shell replacement, a wire session's
/// detach, a batch script's last line — without a host call site.
impl Drop for LocalState {
    fn drop(&mut self) {
        if self.workers_owned {
            self.workers.cancel_all();
        }
    }
}

/// The runtime: a field changes within a run (`io`), survives a run
/// ([`SessionState`]), crosses evaluation boundaries ([`Mobile`]), or is host
/// scratch ([`LocalState`]).
///
/// Every field is `pub(crate)`, because the partition encodes run safety,
/// capability attenuation, and mobile framing — core's invariant, not an API a
/// host may reach past.  Hosts drive a session through the intent verbs
/// ([`mod@host`], plus the scope and context verbs), and the run door
/// ([`Shell::run`]) checkpoints and rolls back the [`Mobile`] around each run.
pub struct Shell {
    pub(crate) mobile: Mobile,
    pub(crate) io: Io,
    pub(crate) session: SessionState,
    pub(crate) local: LocalState,
}

impl Shell {
    /// Construct an unspanned [`Error`]: the break path stamps the span of the
    /// innermost node it unwinds through.  A caller already holding a better
    /// span attaches it with [`Error::at_span`].
    pub fn err(&self, msg: impl Into<String>, status: i32) -> Error {
        Error::new(msg, status)
    }

    /// Like [`Self::err`], with an additional hint.
    pub fn err_hint(&self, msg: impl Into<String>, hint: impl Into<String>, status: i32) -> Error {
        Error::new(msg, status).with_hint(hint)
    }

    /// Resolve `span` to the value-typed [`CallSite`] observations and
    /// capability checks carry.  [`CallSite::default`] when there is no span,
    /// or its source is not registered in this session.
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

    /// [`Self::site_of`] applied to the dispatch register [`Audit`] carries.
    /// Returned by value so the caller may hold `&mut audit` alongside it.
    /// `pub`, not `pub(crate)`: a host door that builds its own
    /// [`crate::types::Observation`] (a grep walk, a read outside any
    /// redirect) needs the same call site core's own doors stamp.
    pub fn call_site(&self) -> CallSite {
        self.site_of(self.local.audit.call_site)
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
        req: crate::serial::FOValue,
    ) -> Result<crate::serial::FOValue, crate::types::Error> {
        match mooring.desk.as_ref() {
            Some(desk) => desk.enquire(req, mooring.cancel.as_scope()),
            None => Err(self.err(crate::types::NO_DESK, crate::types::NO_DESK_STATUS)),
        }
    }

    /// Fork this shell ([`Self::fork_session`]) for a child engine to inherit.
    ///
    /// The one snapshot law lands here: the fork's scope is scrubbed of
    /// `Value::Handle` bindings, so an identity-adopted child and a
    /// wire-hatched one — both forked here — resolve every name to the same
    /// value or the same absence.
    pub fn fork_scrubbed(&self) -> Self {
        let mut fork = self.fork_session();
        fork.mobile.scope = fork.mobile.scope.scrub_handles();
        fork
    }

    /// [`Self::fork_scrubbed`] plus the in-process hand-off: park the fork in
    /// `mooring`'s nursery and return the [`NurseryId`] a desk handler — barred
    /// by the reentrancy law from holding `&mut Shell` itself — later redeems
    /// with `Nursery::adopt`.
    ///
    /// # Errors
    /// Returns `Err` unless this run's [`Fork`] door is [`Fork::Park`].
    pub fn fork_into_nursery(&self, mooring: &Mooring) -> crate::types::Settled<NurseryId> {
        match mooring.fork.as_ref() {
            Some(Fork::Park(nursery)) => Ok(nursery.park(self.fork_scrubbed())),
            Some(Fork::Listen) => Err(crate::types::Break::Error(self.err(
                "this host's forked sessions leave over a wire, so there is no pen to park one in",
                1,
            ))),
            None => Err(crate::types::Break::Error(
                self.err("this host adopts no forked sessions", 1),
            )),
        }
    }

    /// Overwrite `path` through core's whole `>`-redirect recipe —
    /// symlink-resolved, mode-preserving, fsync-durable — while emitting no io
    /// event: the door for a host builtin (exarch's `edit-hash` /
    /// `edit-replace`) that writes *below* the redirect frame and speaks its
    /// own surface, so that it shares the recipe instead of forking a weaker
    /// write that narrows the mode, replaces symlinks, and skips the flush.
    ///
    /// # Errors
    /// Returns `Err` if the target cannot be opened, the write fails, or the
    /// atomic commit (rename and fsync) fails.
    pub fn atomic_write(&mut self, path: &str, bytes: &[u8]) -> crate::types::Settled<()> {
        crate::runtime::command::atomic_write(path, bytes, self)
    }

    /// Register `text` under display `name`, returning the [`FileId`] the
    /// compiled program's spans must carry to resolve back to it.
    pub fn install_script_context(&mut self, name: &str, text: &str) -> FileId {
        self.session.sources.register(Source::from_text(name, text))
    }

    /// Register a top-level run's script and point [`SessionState::root_file`]
    /// at it.  [`crate::run::run_framed`] alone calls this; a module load takes
    /// [`Self::install_script_context`], which appends without disturbing
    /// `root_file`, so the run's own root keeps its name.
    pub fn install_root_context(&mut self, name: &str, text: &str) -> FileId {
        let file = self.install_script_context(name, text);
        self.session.root_file = file;
        file
    }

    /// Register under an already-minted [`FileId`], so a re-exec'd
    /// pipeline-stage child resolves its spans against the exact file identity
    /// its parent handed it, rather than minting a second, differently
    /// numbered copy in its own empty registry.
    pub(crate) fn install_remote_context(&mut self, name: &str, file: FileId, text: &str) {
        self.session
            .sources
            .register_at(file, Source::from_text(name, text));
    }

    /// Resolve the five pseudo-variables (`$ENV`, `$ARGS`, `$NPROC`, `$CWD`,
    /// `$USER`); any other name is `None`.  `$SCRIPT` is not among them: the
    /// elaborator bakes it to a literal from the file it compiles, so no
    /// runtime reader exists.  These are computed, never stored in scope,
    /// hence [`Self::lookup_value_name`] reaching them only after lexical
    /// scope (natives included) misses.
    pub fn pseudo_var(&self, name: &str) -> Option<Value> {
        match name {
            "ENV" => {
                // The host's PWD / OLDPWD are stale the moment ral `cd`s:
                // `context.cwd` is the live pair.  Drop them at the source.
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
            // A pseudo-var is total, so these two name their own placeholder for
            // a host fact that is absent — the reader hands back no stand-in.
            "CWD" => {
                let p = self.cwd();
                let home = self.mobile.context.home();
                let cwd_str = crate::path::abbreviate_home(&p, home.as_deref());
                let cwd_str = if cwd_str.is_empty() {
                    "?".into()
                } else {
                    cwd_str
                };
                Some(Value::String(cwd_str))
            }
            "USER" => Some(Value::String(
                crate::path::user_name(self.mobile.context.env_overrides())
                    .unwrap_or_else(|| "?".into()),
            )),
            _ => None,
        }
    }

    /// Resolve `name` at value position (`$name` and other
    /// [`crate::ir::Val::Variable`] uses).  Binding-only: user aliases and
    /// `within` handlers are operation handlers, not first-class values, so no
    /// lookup here reaches the handler stack.  A value builtin is a plain env
    /// hit — the native scope entry *is* the value — and a base frame has no
    /// value form to find.
    pub fn lookup_value_name(&self, name: &str) -> Option<Value> {
        if let Some(v) = self.mobile.scope.get(name) {
            return Some(v.clone());
        }
        self.pseudo_var(name)
    }

    /// Set `last_status` from a boolean (`true` → 0, `false` → 1).
    #[inline]
    pub fn set_status_from_bool(&mut self, ok: bool) {
        self.mobile.control.last_status = i32::from(!ok);
    }

    /// Write `bytes` to the current stdout sink.
    ///
    /// # Errors
    /// Returns `Err` if the underlying write fails with anything other than
    /// `BrokenPipe`, which is a clean shutdown rather than a fault.
    pub fn write_stdout(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        Self::write_sink(&mut self.io.stdout, bytes)
    }

    /// Write `bytes` to the current stderr sink — where `warn` and the shell's
    /// own diagnostics land, and what `2> f` rebinds.
    ///
    /// # Errors
    /// Returns `Err` if the underlying write fails with anything other than
    /// `BrokenPipe`, which is a clean shutdown rather than a fault.
    pub fn write_stderr(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        Self::write_sink(&mut self.io.stderr, bytes)
    }

    /// `BrokenPipe` is a clean shutdown, as it is for a Unix tool dying
    /// silently on `SIGPIPE`: the reader has closed its end (`fzf` took a
    /// selection, `head` its quota).  Report it and the pipeline supervisor
    /// tears the pgid down with `SIGKILL`, surfacing status 137 on sibling
    /// stages that had themselves exited cleanly.
    fn write_sink(sink: &mut crate::io::Sink, bytes: &[u8]) -> std::io::Result<()> {
        match sink.write_all(bytes) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Snapshot the scope chain for closure capture.  `Arc` so that every
    /// closure taken from one snapshot — a `letrec` bank, say — shares the one
    /// allocation, and later thunk clones are refcount bumps.
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
        assert!(shell.lookup_value_name("USER").is_some());
    }

    /// A `Fork::Listen` run has no pen, and says so rather than reusing the
    /// absent-host sentence: the two are different situations.
    #[test]
    fn fork_into_nursery_refuses_a_listening_run_with_its_own_sentence() {
        let shell = Shell::new(crate::io::TerminalState::default());
        let mooring = Mooring {
            fork: Some(Fork::Listen),
            ..Mooring::adrift()
        };
        match shell
            .fork_into_nursery(&mooring)
            .expect_err("a listening run parks nothing")
        {
            crate::types::Break::Error(e) => {
                assert_eq!(
                    e.message,
                    "this host's forked sessions leave over a wire, so there is no pen to park one in"
                );
            }
            other @ crate::types::Break::Escape(_) => {
                panic!("expected Break::Error, got {other:?}")
            }
        }
    }

    /// The nursery twin of `enquire`'s absent-desk contract, error text and all.
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

    /// Park then adopt returns a live child `Shell` still carrying the
    /// parent's whole lexical scope, as `fork_session` promises.
    #[test]
    fn nursery_round_trips_a_forked_session() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell
            .mobile
            .scope
            .set("parent_binding".to_string(), Value::Int(42));
        let nursery = Nursery::default();
        let mooring = Mooring {
            fork: Some(Fork::Park(nursery.clone())),
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

    /// The run door's nursery-emptying promise holds on unwind too:
    /// `NurseryGuard`'s `Drop` fires whether the body panics or returns, so a
    /// fork parked in `pre_exec` and never adopted cannot outlive the run.
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
                trail: None,
            },
            surface: None,
            deferred: None,
            desk: None,
            fork: Some(Fork::Park(nursery.clone())),
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
