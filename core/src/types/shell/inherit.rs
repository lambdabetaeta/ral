//! Moving state from a parent shell into a child computation.
//!
//! A same-thread β-step is no fork: [`Shell::with_thunk_body`] runs the body
//! on the caller's [`Shell`], swapping only the [`Mobile`] for one rescoped to
//! the closure's captured environment, while `io`,
//! [`SessionState`](super::SessionState) and [`LocalState`](super::LocalState)
//! stay shared by identity — no second store to drift from the first.
//!
//! The owned-[`Shell`] routines below — spawned thread, cross-process pipeline
//! stage, REPL aside, sub-agent session — are genuine forks over a different
//! store, so each spells out what it carries across.  Every one starts from a
//! defaulted [`SessionState`](super::SessionState), which mints no
//! [`TerminalLease`](crate::process::TerminalLease): the foreground gate wants
//! the run's access *and* the session's lease, so a fork fails the second half
//! whatever [`Mooring`] it later runs under.

use super::{Mobile, Mooring, Shell, SurfaceSink};
use crate::types::{ControlState, Env};
use std::sync::Arc;

/// Which same-thread thunk body [`Shell::with_thunk_body`] is eliminating.
/// A forced block and an applied lambda differ in exactly two places: the
/// entry `last_status` and the fold-back set.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ThunkBody {
    /// `!{ … }` — enters with the caller's `$?` and folds only `last_status`
    /// back, so a block's `let` / `cd` and any `chpwd` it queues die with the
    /// body mobile.
    Block,
    /// `λx. …` applied to an argument: a fresh `$?` on entry, `{last_status,
    /// cwd}` folded back, so a `cd` inside a function, alias, or handler
    /// persists like every other shell.  The caller's closure binds the
    /// parameter — pattern binding lives in the evaluator, above `Shell`.
    Lambda,
}

impl Shell {
    /// Clone the persistable half of this shell: the bundle that crosses an
    /// evaluation boundary, leaving `io`, audit and REPL scratch behind.
    pub fn mobile(&self) -> Mobile {
        self.mobile.clone()
    }

    /// Replace `self.mobile` wholesale; snapshot with [`Self::mobile`] to keep
    /// what it displaces.
    ///
    /// `pub(crate)` so nothing outside core can overwrite the handler stack
    /// behind the evaluator's back.  A wire-borne mobile goes through
    /// [`crate::subprocess::install_shell_mobile`], which splices the wire's
    /// handler frames atop the receiver's own.
    pub(crate) fn install_mobile(&mut self, mobile: Mobile) {
        self.mobile = mobile;
    }

    /// Swap `mobile` in, run `f`, swap back out; the post-run bundle comes
    /// back beside `f`'s result, to keep or to discard.
    ///
    /// An unwind through `f` would leave the passed-in bundle installed —
    /// harmless only because ral carries control flow in its own signals, not
    /// in Rust panics.
    pub fn run_with_mobile<R>(
        &mut self,
        mobile: Mobile,
        f: impl FnOnce(&mut Self) -> R,
    ) -> (Mobile, R) {
        let saved = std::mem::replace(&mut self.mobile, mobile);
        let result = f(self);
        let post = std::mem::replace(&mut self.mobile, saved);
        (post, result)
    }

    /// Evaluate a same-thread thunk body — a forced block or an applied
    /// lambda — in place on this shell.  The single routine block and lambda
    /// elimination meet at: `f` installs the body's [`Mobile`] and hands it
    /// back, and the [`ThunkBody`]-specific set is folded onto the caller's.
    ///
    /// `local.repl.pending_chpwd` is bracketed for a [`ThunkBody::Block`] — a
    /// block has no business persisting a REPL notification the parent would
    /// replay — and left to ride the shared `local.repl` for a
    /// [`ThunkBody::Lambda`], the notification analogue of its `cwd` fold-back.
    pub(crate) fn with_thunk_body<R>(
        &mut self,
        kind: ThunkBody,
        captured: &Env,
        f: impl FnOnce(&mut Self, Mobile) -> (Mobile, R),
    ) -> R {
        let mut mobile = self.mobile.clone();
        mobile.scope = captured.clone();
        mobile.scope.push_scope();
        if matches!(kind, ThunkBody::Lambda) {
            mobile.control.last_status = ControlState::default().last_status;
        }
        let saved_pending_chpwd = match kind {
            ThunkBody::Block => self.local.repl.pending_chpwd.take(),
            ThunkBody::Lambda => None,
        };
        let (post, result) = f(self, mobile);
        if matches!(kind, ThunkBody::Block) {
            self.local.repl.pending_chpwd = saved_pending_chpwd;
        }
        self.mobile.control.last_status = post.control.last_status;
        if matches!(kind, ThunkBody::Lambda) {
            self.mobile.context.cwd.current = post.context.cwd.current;
            self.mobile.context.cwd.previous = post.context.cwd.previous;
        }
        result
    }

    /// A defaulted [`Shell`] scoped to `captured`: no inherited grants, env
    /// vars, or call site.  The base every fork below builds on.
    fn from_captured(captured: &Env) -> Self {
        let mut shell = Self::new(crate::io::TerminalState::default());
        shell.mobile.scope = captured.clone();
        shell
    }

    /// Cross-process pipeline-stage child: inherit `parent`'s context *and*
    /// move its read-once bits — pipe stdin, audit trail, REPL editor context
    /// — into the child.  Pair with [`Shell::return_to`], lest the lent state
    /// die with the child.
    ///
    /// `parent` is the throwaway `child_eval` rebuilds in the helper process,
    /// so the loan is repaid into that throwaway and not a live caller.
    pub fn child_of(captured: &Env, parent: &mut Self) -> Self {
        let mut child = Self::from_captured(captured);
        child.inherit_from(parent);
        child
    }

    /// Clone `parent`'s context into an independent sibling, touching none of
    /// its IO, audit, or REPL editor context — no flow-back needed.  The
    /// shared body of [`Self::fork_session`] and [`Self::join_session`].
    ///
    /// The builtin table, library docs, call site and source registry ride
    /// along so the child resolves, renders, and describes as the parent does;
    /// the detach budget too, so a child that resolves `detach` spends the
    /// parent's births rather than a fresh allowance.
    pub fn child_from(captured: &Env, parent: &Self) -> Self {
        let mut child = Self::from_captured(captured);
        child.mobile.context = parent.mobile.context.clone();
        child.local.audit.call_site = parent.local.audit.call_site;
        // Rides with the call site: without the registry, the child's spans
        // name sources it does not hold.
        child.session.sources = parent.session.sources.clone();
        child.session.builtins = parent.session.builtins.clone();
        child
            .session
            .library_docs
            .clone_from(&parent.session.library_docs);
        child
            .session
            .guest_jail
            .clone_from(&parent.session.guest_jail);
        child.local.detach.clone_from(&parent.local.detach);
        child
    }

    /// Fork this shell into an independent child *session* — the primitive a
    /// host uses to spawn a sub-agent that executes its own runs.
    ///
    /// The session-scoped [`Self::child_from`]: scope, context, and builtin
    /// table are snapshotted, everything else fresh — control counters, since
    /// a new session continues no call stack, and a durable root deaf to the
    /// ambient causes even when forked from a facing session, its host
    /// cancelling it through [`Shell::cancel_handle`] instead.  Nothing flows
    /// back: the child's `cd`, env, and new bindings die with it.
    ///
    /// Routing host forks through one door keeps "what a child inherits" in
    /// one place, where a hand-copying call site would quietly sever a datum
    /// it forgot.
    pub fn fork_session(&self) -> Self {
        Self::child_from(&self.mobile.scope, self)
    }

    /// Join this session as an *aside*: a second [`Shell`] the host runs
    /// beside it rather than as one — the REPL's hook shell, evaluating
    /// arbitrary plugin code while the session sits at its prompt.
    ///
    /// The twin of [`Self::fork_session`], opposite in the one place that
    /// matters: it *shares* this session's durable root instead of minting a
    /// fresh one, so it is inside the session for cancellation.  An interrupt
    /// aimed at a command the session was already running is older than every
    /// frame the aside will mint, so the aside can neither absorb it nor keep
    /// it from the run it was aimed at.
    pub fn join_session(&self) -> Self {
        let mut aside = Self::child_from(&self.mobile.scope, self);
        aside.session.root = self.session.root.clone();
        aside.session.anchor = aside.session.root.worker();
        aside
    }

    /// Spawn `f` on a fresh OS thread with a cloned child shell — the one and
    /// only thread-spawn primitive.  `scopes` is the thunk's captured closure
    /// scope and `surface` the buffering sink the worker surfaces into instead
    /// of the spawning run's live one; per-fork IO setup lives inside `f`.
    ///
    /// The worker's counters start fresh, since it continues no call stack,
    /// but the `recursion_limit` ceiling is the parent's: a limit set by rc or
    /// CLI belongs to the session, not to one stack.
    ///
    /// Its [`Mooring`] is a *rebuild* ([`Mooring::for_worker`]), never a
    /// share, minted here rather than in the thread so the worker's cancel
    /// scope can be returned: the worker hangs off a
    /// [`worker`](crate::process::DurableRoot::worker) scope of the durable
    /// root, so a foreground cancel — a run timeout, a Ctrl-C — misses it,
    /// while the ambient shutdown cause folded through the root, a
    /// [`RootAbort`](crate::process::CancelCause::RootAbort), or a cancel on
    /// the returned scope stops it.
    ///
    /// The worker registry and the detach budget are `Arc`-shared, not copied,
    /// so a `spawn` or `detach` nested in `f`'s body registers and spends
    /// where this shell's own do.
    pub fn spawn_thread<F, R>(
        &self,
        parent: &Mooring,
        surface: SurfaceSink,
        scopes: Arc<Env>,
        f: F,
    ) -> (std::thread::JoinHandle<R>, crate::process::CancelScope)
    where
        F: FnOnce(&Mooring, &mut Self) -> R + Send + 'static,
        R: Send + 'static,
    {
        let context = self.mobile.context.clone();
        let recursion_limit = self.mobile.control.recursion_limit;
        let root = self.session.root.clone();
        let builtins = self.session.builtins.clone();
        let library_docs = self.session.library_docs.clone();
        let guest_jail = self.session.guest_jail.clone();
        let workers = self.local.workers.clone();
        let detach = self.local.detach.clone();
        let mooring = Mooring::for_worker(parent, &root, surface);
        let worker_cancel = mooring.cancel.as_scope().clone();
        let handle = std::thread::spawn(move || {
            let mut child = Self::from_captured(&scopes);
            child.mobile.context = context;
            child.mobile.control.recursion_limit = recursion_limit;
            child.session.anchor = mooring.cancel.clone();
            child.session.root = root;
            child.session.builtins = builtins;
            child.session.library_docs = library_docs;
            child.session.guest_jail = guest_jail;
            child.local.workers = workers;
            child.local.detach = detach;
            // Shared, not owned: this worker's shell dropping must not cancel
            // the parent's whole registry.
            child.local.workers_owned = false;
            f(&mooring, &mut child)
        });
        (handle, worker_cancel)
    }

    /// Propagate `parent`'s state into this cross-process pipeline-stage child;
    /// [`Self::child_of`] is its only caller.  Each substate carries its own
    /// inherit rule, whose asymmetry with [`Self::return_to`] is the flow
    /// matrix.
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.mobile.context = parent.mobile.context.clone();
        self.mobile.control.inherit_from(&parent.mobile.control);
        self.io.inherit_from(&mut parent.io);
        self.local.audit.inherit_from(&mut parent.local.audit);
        self.local.repl.inherit_from(&mut parent.local.repl);
        self.session.builtins = parent.session.builtins.clone();
        self.session.library_docs = parent.session.library_docs.clone();
        self.session.root = parent.session.root.clone();
        self.session.anchor = parent.session.anchor.clone();
        self.session.guest_jail = parent.session.guest_jail.clone();
    }

    /// Flow a child stage's mutations back to `parent`.  The call site and the
    /// `within`-attenuable bits stay behind; both halves of `cwd` do not, so a
    /// `cd` in a stage persists like every other shell.  A spawned thread
    /// never runs this, so its own `cd`s stay private.
    pub fn return_to(&mut self, parent: &mut Self) {
        self.mobile.control.return_to(&mut parent.mobile.control);
        self.local.audit.return_to(&mut parent.local.audit);
        self.local.repl.return_to(&mut parent.local.repl);
        self.io.return_to(&mut parent.io);
        parent.mobile.context.cwd.current = self.mobile.context.cwd.current.take();
        parent.mobile.context.cwd.previous = self.mobile.context.cwd.previous.take();
    }
}

// Unix-only: the lease tests assert a minted `TerminalLease` is `Some`, and
// `mint_at_startup` returns `None` unconditionally where there is no
// `tcsetpgrp`.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::process::TerminalLease;
    use crate::types::shell::{DEFAULT_RECURSION_LIMIT, TerminalAccess};

    /// A same-thread lambda body runs in place on the caller's shell, so it
    /// observes the session-owned terminal lease *by identity*: a foreground
    /// external inside a function, alias, or handler body can take the
    /// controlling terminal whenever the run is `Leased`.
    #[test]
    fn lambda_body_shares_the_session_terminal_lease() {
        let mut shell = Shell::default();
        shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
        let mooring = Mooring {
            terminal_access: TerminalAccess::Leased,
            ..Mooring::adrift()
        };
        assert!(
            shell.terminal_lease(&mooring).is_some(),
            "precondition: the session holds a Leased lease",
        );

        let captured = shell.mobile.scope.clone();
        shell.with_thunk_body(ThunkBody::Lambda, &captured, |shell, mobile| {
            shell.run_with_mobile(mobile, |body| {
                assert!(
                    body.terminal_lease(&mooring).is_some(),
                    "a Leased lambda body shares the session lease",
                );
            })
        });

        assert!(
            shell.terminal_lease(&mooring).is_some(),
            "the session still holds the lease after the body — it never moved",
        );
    }

    /// A forked session builds over a defaulted `SessionState` and so mints no
    /// lease witness: a sub-agent can never foreground an external command and
    /// seize the controlling terminal the host's TUI owns, even forked from a
    /// parent that holds the lease and run under a mooring claiming `Leased`.
    #[test]
    fn fork_session_holds_no_terminal_authority() {
        let mut parent = Shell::default();
        parent.session.terminal_lease = TerminalLease::mint_at_startup(true);
        let mooring = Mooring {
            terminal_access: TerminalAccess::Leased,
            ..Mooring::adrift()
        };
        assert!(
            parent.terminal_lease(&mooring).is_some(),
            "precondition: the parent holds a Leased lease",
        );

        let child = parent.fork_session();
        assert!(
            child.terminal_lease(&mooring).is_none(),
            "a forked session minted no lease witness, so it cannot foreground",
        );
    }

    /// An rc `recursion_limit:` key or a `--recursion-limit` flag configures
    /// the session, so a `spawn` / `par` / `watch` body must not silently fall
    /// back to the compile-time default — though its counters do start fresh.
    #[test]
    fn spawned_worker_inherits_the_recursion_limit() {
        let mut parent = Shell::default();
        parent.set_recursion_limit(DEFAULT_RECURSION_LIMIT + 7);
        let scopes = Arc::new(parent.mobile().scope);
        let (join, _cancel) =
            parent.spawn_thread(&Mooring::adrift(), Arc::new(()), scopes, |_, child| {
                child.mobile.control.clone()
            });

        let control = join.join().expect("worker thread");
        assert_eq!(control.recursion_limit, DEFAULT_RECURSION_LIMIT + 7);
        assert_eq!(control.call_depth, 0, "a worker starts a fresh call stack");
        assert_eq!(control.last_status, 0, "a worker starts with a fresh `$?`");
    }
}
