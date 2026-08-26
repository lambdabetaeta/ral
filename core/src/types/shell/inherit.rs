//! Moving state from a parent shell into a child computation.
//!
//! The owned-[`Shell`] routines below — spawned thread, cross-process pipeline
//! stage, REPL aside, sub-agent session — are genuine forks over a different
//! store, so each spells out what it carries across.  Every one starts from a
//! defaulted [`SessionState`](super::SessionState), which mints no
//! [`TerminalLease`](crate::process::TerminalLease): the foreground gate wants
//! the run's access *and* the session's lease, so a fork fails the second half
//! whatever [`Mooring`] it later runs under.

use super::{Mooring, Shell, SurfaceSink};
use crate::types::Env;
use std::sync::Arc;

impl Shell {
    /// A defaulted [`Shell`] scoped to `captured`: no inherited grants, env
    /// vars, or call site.  The base every fork below builds on.
    fn from_captured(captured: &Env) -> Self {
        let mut shell = Self::new(crate::io::TerminalState::default());
        shell.env = captured.clone();
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
        child.context = parent.context.clone();
        child.local.audit.call_site = parent.local.audit.call_site;
        // Rides with the call site: without the registry, the child's spans
        // name sources it does not hold.
        child.session.sources = parent.session.sources.clone();
        child.session.builtins = parent.session.builtins.clone();
        child.session.stack_limit = parent.session.stack_limit;
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
        Self::child_from(&self.env, self)
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
        let mut aside = Self::child_from(&self.env, self);
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
    /// but the `stack_limit` ceiling is the parent's: a limit set by rc or
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
        let context = self.context.clone();
        let stack_limit = self.session.stack_limit;
        let root = self.session.root.clone();
        let builtins = self.session.builtins.clone();
        let library_docs = self.session.library_docs.clone();
        let guest_jail = self.session.guest_jail.clone();
        let workers = self.local.workers.clone();
        let detach = self.local.detach.clone();
        let mooring = Mooring::for_worker(parent, &root, surface);
        let worker_cancel = mooring.cancel.as_scope().clone();
        let live = self.local.workers.live_ticket();
        let handle = std::thread::spawn(move || {
            let mut child = Self::from_captured(&scopes);
            child.context = context;
            child.session.stack_limit = stack_limit;
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
            let out = f(&mooring, &mut child);
            // The ticket goes last, after the body and its shell, so a
            // teardown's drain outlasts this frame's children rather than
            // merely seeing the cancel land.
            drop((child, live));
            out
        });
        (handle, worker_cancel)
    }

    /// Propagate `parent`'s state into this cross-process pipeline-stage child;
    /// [`Self::child_of`] is its only caller.  Each substate carries its own
    /// inherit rule, whose asymmetry with [`Self::return_to`] is the flow
    /// matrix.
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.context = parent.context.clone();
        self.io.inherit_from(&mut parent.io);
        self.local.audit.inherit_from(&mut parent.local.audit);
        self.local.repl.inherit_from(&mut parent.local.repl);
        self.session.builtins = parent.session.builtins.clone();
        self.session.library_docs = parent.session.library_docs.clone();
        self.session.stack_limit = parent.session.stack_limit;
        self.session.root = parent.session.root.clone();
        self.session.anchor = parent.session.anchor.clone();
        self.session.guest_jail = parent.session.guest_jail.clone();
    }

    /// Flow a child stage's mutations back to `parent`.  The call site and the
    /// `within`-attenuable bits stay behind; both halves of `cwd` do not, so a
    /// `cd` in a stage persists like every other shell.  A spawned thread
    /// never runs this, so its own `cd`s stay private.
    pub fn return_to(&mut self, parent: &mut Self) {
        parent.last_status = self.last_status;
        self.local.audit.return_to(&mut parent.local.audit);
        self.local.repl.return_to(&mut parent.local.repl);
        self.io.return_to(&mut parent.io);
        parent.context.cwd.current = self.context.cwd.current.take();
        parent.context.cwd.previous = self.context.cwd.previous.take();
    }
}

// Unix-only: the lease tests assert a minted `TerminalLease` is `Some`, and
// `mint_at_startup` returns `None` unconditionally where there is no
// `tcsetpgrp`.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::process::TerminalLease;
    use crate::types::shell::{DEFAULT_STACK_LIMIT, TerminalAccess};

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
    /// the session as `session.stack_limit`, so a `spawn` / `par` / `watch`
    /// body must not silently fall back to the compile-time default — though
    /// `last_status` does start fresh.
    #[test]
    fn spawned_worker_inherits_the_stack_limit() {
        let mut parent = Shell::default();
        parent.set_stack_limit(DEFAULT_STACK_LIMIT + 7);
        let scopes = Arc::new(parent.env.clone());
        let (join, _cancel) =
            parent.spawn_thread(&Mooring::adrift(), Arc::new(()), scopes, |_, child| {
                (child.session.stack_limit, child.last_status)
            });

        let (stack_limit, last_status) = join.join().expect("worker thread");
        assert_eq!(stack_limit, DEFAULT_STACK_LIMIT + 7);
        assert_eq!(last_status, 0, "a worker starts with a fresh `$?`");
    }
}
