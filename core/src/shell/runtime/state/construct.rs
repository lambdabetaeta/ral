//! The one session constructor.  A shell's state has three sources: the
//! binary ([`Runtime`]), the parent or nothing ([`Image`]), and the process
//! itself, made inside [`Shell::construct`].

use super::bindings::{BindingLease, BindingLedger};
use super::hooks::HookTable;
use super::modules::Modules;
use super::workers::Roster;
use super::{Context, DEFAULT_STACK_LIMIT, LocalState, SessionState, Shell};
use crate::authority::policy::{Baseline, GrantStack};
use crate::host::EnvVars;
use crate::io::terminal::TerminalState;
use crate::io::{Io, Source, SourceReader};
use crate::path::CwdPath;
use crate::process::{DurableRoot, Scope, TerminalLease};
use crate::shell::grant::{GrantNarrower, SpawnGrant};
use crate::shell::runtime::handler::{HandlerFrame, HandlerStack};
use crate::shell::runtime::{
    Audit, BuiltinEntry, BuiltinTable, Convention, Cradle, Env, Error, ExitHints, Set, Signature,
    Value,
};
use crate::source::SourceDb;
use crate::ty::Fixings;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// The process half of a session: what the binary supplies through the
/// installer.  Shared through its `Arc`s by every session an engine
/// constructs in process; a child engine derives its own.
#[derive(Clone)]
pub struct Runtime {
    pub(crate) sig: Arc<Signature>,
    pub(crate) builtins: BuiltinTable,
    /// The base handler layer the builtin table's argv half installs.
    pub(crate) base: Vec<BuiltinEntry>,
    pub(crate) library_docs: HashMap<String, String>,
    pub(crate) terminal: TerminalState,
    /// fd 0 as the process found it: `Terminal` at a tty, else a reader over
    /// one close-on-exec duplicate, `Empty` where fd 0 is closed.  Every
    /// run's inherited stdin is minted from it.
    stdin: Arc<Source>,
    /// Host system reads and boot-image protection, outside the grant stack.
    baseline: Baseline,
}

impl Runtime {
    /// No builtins, no prelude, no docs, over the baseline of this host.
    ///
    /// # Errors
    /// A system read that is malformed or whose walk fails, named: a host
    /// that cannot resolve its own baseline cannot run anything.
    pub fn bare(terminal: TerminalState) -> Result<Self, Error> {
        Ok(Self::bare_on(Baseline::of_host()?, terminal))
    }

    /// [`bare`](Self::bare) over a baseline already resolved.
    pub(crate) fn bare_on(baseline: Baseline, terminal: TerminalState) -> Self {
        Self {
            sig: Arc::default(),
            builtins: BuiltinTable::default(),
            base: Vec::new(),
            library_docs: HashMap::new(),
            terminal,
            stdin: Arc::new(fd0(terminal)),
            baseline,
        }
    }

    /// A run's inherited stdin: the terminal, or a duplicate of fd 0's reader.
    ///
    /// # Errors
    /// Returns `Err` if duplicating the reader fails.
    pub(crate) fn stdin(&self) -> std::io::Result<Source> {
        Ok(match &*self.stdin {
            Source::Terminal => Source::Terminal,
            Source::Empty => Source::Empty,
            Source::Reader(r) => Source::Reader(r.try_clone()?),
        })
    }

    /// Host facts supplied to every authority judgment.
    pub fn baseline(&self) -> &Baseline {
        &self.baseline
    }

    pub fn install_builtins(&mut self, entries: &'static [BuiltinEntry]) {
        self.install_set(&Set::Static(entries));
    }

    pub fn install_captured_builtins(&mut self, entries: &Arc<[BuiltinEntry]>) {
        self.install_set(&Set::Captured(Arc::clone(entries)));
    }

    /// Install `set`, then its natives into Σ and its argv half into `base`,
    /// unless it was already here.
    pub(crate) fn install_set(&mut self, set: &Set) {
        if self.builtins.install(set.clone()) {
            let (natives, base) = natives_and_base(set);
            Arc::make_mut(&mut self.sig).install_natives(natives);
            self.base.extend(base);
        }
    }

    pub fn install_library_docs(&mut self, entries: Vec<(String, String)>) {
        self.library_docs.extend(entries);
    }
}

/// fd 0 as a [`Source`], once, so `Terminal` means a terminal.
fn fd0(terminal: TerminalState) -> Source {
    if terminal.startup_stdin_tty {
        return Source::Terminal;
    }
    dup_stdin().map_or(Source::Empty, |f| Source::Reader(SourceReader::file(f)))
}

#[cfg(unix)]
fn dup_stdin() -> std::io::Result<std::fs::File> {
    use std::os::fd::AsFd;
    std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map(std::fs::File::from)
}

#[cfg(windows)]
fn dup_stdin() -> std::io::Result<std::fs::File> {
    use std::os::windows::io::AsHandle;
    std::io::stdin()
        .as_handle()
        .try_clone_to_owned()
        .map(std::fs::File::from)
}

/// Freshly installed `entries` split by the manifest's two halves: a value
/// row is a native for Σ, an argv row a base handler.  Each row says which it
/// is ([`Convention`]); the one place either is populated.
pub(super) fn natives_and_base(
    entries: &[BuiltinEntry],
) -> (Vec<(String, Value)>, Vec<BuiltinEntry>) {
    let natives = entries
        .iter()
        .filter(|entry| entry.decl.convention == Convention::Value)
        .map(|entry| {
            (
                entry.decl.name.clone().into_owned(),
                crate::shell::runtime::builtin::native_value(entry),
            )
        })
        .collect();
    let base = entries
        .iter()
        .filter(|entry| entry.decl.convention == Convention::Argv)
        .cloned()
        .collect();
    (natives, base)
}

/// Session policy: one type in memory and on the wire, so a term added here
/// reaches both seats or the build fails.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Terms {
    pub(crate) stack_limit: usize,
    pub(crate) child_limits: crate::process::ChildLimits,
    pub(crate) exit_hints: ExitHints,
    /// The spelling of the session's scratch directory, if its front end made
    /// one: what a `scratch` place anchors to.
    pub(crate) scratch: Option<String>,
    pub(crate) lease: Option<BindingLease>,
    pub(crate) retention: Option<u64>,
}

impl Terms {
    /// A root's: the compile-time stack cap and nothing armed; the root
    /// recipe arms what it wants.
    fn root() -> Self {
        Self {
            stack_limit: DEFAULT_STACK_LIMIT,
            child_limits: crate::process::ChildLimits::default(),
            exit_hints: ExitHints::default(),
            scratch: None,
            lease: None,
            retention: None,
        }
    }

    /// This session's, as it stands.
    pub(super) fn of(shell: &Shell) -> Self {
        Self {
            stack_limit: shell.session.stack_limit,
            child_limits: shell.session.child_limits,
            exit_hints: ExitHints::clone(&shell.session.exit_hints),
            scratch: shell.session.scratch.clone(),
            lease: shell.local.bindings.lease(),
            retention: shell.local.workers.retention(),
        }
    }
}

/// What only a root session owns, beside its image: a bequest cannot express
/// either.
pub(crate) struct RootOnly {
    hooks: HookTable,
    terminal_lease: Option<TerminalLease>,
}

impl RootOnly {
    /// An empty hook table, and the lease the terminal's startup foreground
    /// mints.
    fn minted(terminal: TerminalState) -> Self {
        Self {
            hooks: HookTable::default(),
            terminal_lease: TerminalLease::mint_at_startup(terminal.startup_foreground),
        }
    }
}

/// A session's own state: exactly what a child can inherit, and what a root
/// starts from empty.
pub(crate) struct Image {
    pub(crate) env: Env,
    /// Handler frames only; the base layer is the runtime's.
    pub(crate) context: Context<Vec<HandlerFrame>>,
    pub(crate) sources: SourceDb,
    pub(crate) terms: Terms,
    pub(crate) fixings: Fixings,
}

/// What a child takes from its parent: its image, scrubbed, and the spawn's
/// grant.  The nursery parks one; the host relays one sealed as `EngineSeed`.
pub struct Bequest {
    pub(crate) image: Image,
    pub(crate) grant: SpawnGrant,
}

impl Image {
    /// Empty scope at `cwd`, root grants, no sources, `Terms::root()`, no
    /// fixings.
    fn empty(cwd: CwdPath) -> Self {
        Self {
            env: Env::new(),
            context: Context {
                env_overrides: EnvVars::default(),
                cwd,
                grants: GrantStack::root(),
                handlers: Vec::new(),
                args: Vec::new(),
                modules: Modules::default(),
            },
            sources: SourceDb::default(),
            terms: Terms::root(),
            fixings: Fixings::default(),
        }
    }
}

impl Shell {
    /// The one session literal, written without `..`.  `root` is `Some` for a
    /// parentless session and `None` for a child.
    fn construct(runtime: Arc<Runtime>, image: Image, root: Option<RootOnly>) -> Self {
        let sig = runtime.sig.clone();
        let Image {
            env,
            context,
            sources,
            terms,
            fixings,
        } = image;
        let Terms {
            stack_limit,
            child_limits,
            exit_hints,
            scratch,
            lease,
            retention,
        } = terms;
        let (hooks, terminal_lease) = match root {
            Some(RootOnly {
                hooks,
                terminal_lease,
            }) => (Some(hooks), terminal_lease),
            None => (None, None),
        };
        let durable = DurableRoot::new();
        let workers = Roster::default();
        if let Some(retention) = retention {
            workers.arm_retention(retention);
        }
        let mut bindings = BindingLedger::default();
        if let Some(lease) = lease {
            let baseline = env.all_bindings(&sig).into_iter().map(|(name, _)| name);
            bindings.arm(lease, baseline);
        }
        let anchor = Scope::clone(&durable.child());
        let cradle = Cradle::loose(Scope::clone(&durable));
        Self {
            env,
            sig,
            context: context
                .map_handlers(|frames| HandlerStack::over(runtime.base.clone(), frames)),
            io: Io::default(),
            scope: anchor.clone(),
            session: SessionState {
                anchor,
                root: durable,
                sources,
                root_file: None,
                exit_hints: Arc::new(exit_hints),
                scratch,
                fixings: Arc::new(fixings),
                runtime,
                terminal_lease,
                stack_limit,
                child_limits,
                interactive: false,
                stage: None,
                hooks,
            },
            local: LocalState {
                audit: Audit::default(),
                workers,
                cradle,
                bindings,
                machine_depth: 0,
            },
        }
    }

    /// A child session over `runtime`, from `bequest`: the one constructor of
    /// a child on either seat.  The grant's layer is pushed last, frozen
    /// against the child's own cwd.
    ///
    /// # Errors
    /// Whatever the grant's narrowing refuses.
    pub(crate) fn child_of(
        runtime: Arc<Runtime>,
        bequest: Bequest,
        narrow: GrantNarrower,
    ) -> Result<Self, Error> {
        let Bequest { image, grant } = bequest;
        let mut child = Self::construct(runtime, image, None);
        grant.narrow_onto(&mut child, narrow)?;
        Ok(child)
    }

    /// A parentless session over `runtime` at `cwd`: no env, no recipe.
    ///
    /// The cwd is the one thing a root cannot start without: every relative
    /// path it reads is read against it, so a front end that has none to give
    /// fails to boot rather than lend one.
    ///
    /// The session faces no signals: its host forwards them as `Control`.
    pub fn root(runtime: Arc<Runtime>, cwd: CwdPath) -> Self {
        let root = RootOnly::minted(runtime.terminal);
        Self::construct(runtime, Image::empty(cwd), Some(root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostSurface;

    #[test]
    fn a_root_owns_its_hooks_and_terminal_lease() {
        let terminal = TerminalState {
            startup_foreground: true,
            ..TerminalState::default()
        };
        let shell = Shell::root(
            HostSurface::default()
                .runtime(terminal)
                .expect("a test host resolves its baseline")
                .into(),
            crate::test_helper::process_cwd(),
        );
        assert!(shell.session.hooks.is_some());
        #[cfg(unix)]
        assert!(shell.session.terminal_lease.is_some());
        assert!(matches!(shell.io.stdin, Source::Empty));
        assert!(!shell.local.bindings.armed());
        assert_eq!(shell.local.workers.retention(), None);
        assert_eq!(shell.session.stack_limit, DEFAULT_STACK_LIMIT);
    }

    /// A run's inherited stdin is the terminal only at a tty; any other fd 0
    /// is a reader, minted afresh per run.
    #[test]
    fn fd0_is_the_terminal_only_at_a_tty() {
        let at_tty = TerminalState {
            startup_stdin_tty: true,
            ..TerminalState::default()
        };
        assert!(matches!(fd0(at_tty), Source::Terminal));
        let runtime = Runtime::bare_on(Baseline::default(), TerminalState::default());
        assert!(matches!(runtime.stdin(), Ok(Source::Reader(_))));
        assert!(matches!(runtime.stdin(), Ok(Source::Reader(_))));
    }
}
