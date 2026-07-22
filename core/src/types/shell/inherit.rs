//! State transfer between parent and child interpreter states.
//!
//! A same-thread β-step — forcing a block or applying a lambda — runs the
//! body *in* the caller's [`Shell`] ([`Shell::with_thunk_body`]): only the
//! [`Mobile`] is swapped for one rescoped to the closure's captured
//! environment, while [`TurnState`](super::TurnState),
//! [`SessionState`](super::SessionState), and
//! [`LocalState`](super::LocalState) are shared by identity.  The body
//! therefore observes the caller's audit trail, byte sinks, builtin table,
//! cancel root, and terminal lease without any of them being copied or
//! re-attached — there is no second store to drift from the first.
//! [`ThunkBody`] fixes the only two places a block and a lambda differ: the
//! entry `last_status` and the fold-back set.
//!
//! The owned-[`Shell`] modes below are *genuine* runtime forks — a
//! different store — and so copy state explicitly:
//!
//! - **Spawned thread** (`spawn`, `par`, detached worker):
//!   [`Shell::spawn_thread`] snapshots the parent's [`Context`](super::Context)
//!   and ships it to a fresh OS thread that owns its own IO; nothing flows
//!   back. The worker registry (`local.workers`) is the one piece of
//!   [`LocalState`](super::LocalState) that *does* flow in here — shared by
//!   `Arc`, not cloned-and-forgotten — so a `spawn` nested inside a worker's
//!   body registers into the same directory its parent did.
//! - **Cross-process pipeline stage**: [`Shell::child_of`] builds a child
//!   over a throwaway parent in the helper process (see
//!   [`crate::child_eval`]) and folds its result back with
//!   [`Shell::return_to`].
//! - **REPL aside** (prompt, hook): [`Shell::child_from`] clones the
//!   parent's [`Context`](super::Context) without touching its local
//!   machinery; the child is an independent sibling with no flow-back.
//! - **Host session fork** (sub-agent): [`Shell::fork_session`] is the
//!   session-scoped specialisation of `child_from` — it snapshots the whole
//!   scope, context, and builtin table into a child session that runs its
//!   own turns, with fresh control counters and no flow-back.
//!
//! Each fork starts from a freshly-defaulted
//! [`SessionState`](super::SessionState) and so holds no terminal authority
//! — `TerminalAccess::Denied`, no lease — the safe default for a store that
//! is not the session's.
//!
//! [`Shell::inherit_from`] and [`Shell::return_to`] are the per-substate
//! manifests the cross-process stage leans on: `mobile.context` clones
//! whole; `mobile.control`, the [`TurnState`](super::TurnState) substates
//! (via [`TurnState::inherit_from`](super::TurnState::inherit_from)
//! / [`return_to`](super::TurnState::return_to)), `local.audit`, and
//! `local.repl` each carry their own inherit / return rule; `session.builtins`,
//! `session.library_docs`, `session.root`, and `session.guest_jail` are shared
//! so dispatch, the `help`/`explain` index, the cancel root, and (on a guest)
//! the jail's uid counter reach the child.  The
//! asymmetry between the two manifests is the flow matrix — the source
//! cursor (`turn.loc`) and the `within`-attenuable bits do not flow back,
//! but `context.cwd` does.

use super::{Mobile, Shell};
use crate::types::{ControlState, Env};
use std::sync::Arc;

/// Which same-thread thunk body [`Shell::with_thunk_body`] is eliminating.
///
/// The kind fixes the two places a forced block and an applied lambda
/// differ — the entry `last_status` and the fold-back set — spelled out in
/// the per-variant docs below.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ThunkBody {
    /// `!{ … }` — force a block (or apply one as a function).  Enters with
    /// the caller's `last_status` (cloned with the mobile) and folds only
    /// `last_status` back: a block's `let` / `cd` and any `chpwd` it queues
    /// die with the body mobile.
    Block,
    /// `λx. …` applied to an argument.  Enters with a *fresh* `last_status`
    /// — a lambda body does not inherit the caller's `$?` — and folds
    /// `{last_status, cwd}` back, so a `cd` inside a function, alias, or
    /// handler persists like every other shell.  The bound parameter is
    /// installed by the caller's closure, since pattern binding lives in the
    /// evaluator, a layer above `Shell`.
    Lambda,
}

impl Shell {
    /// Snapshot the persistable half of this shell's state.
    ///
    /// Returns a clone of [`Self::mobile`] — the bundle that survives
    /// across an evaluation boundary independently of local machinery
    /// (IO, audit, REPL scratch).  The primitive a caller uses to
    /// stash the parent's mobile, install a different one for a
    /// sub-evaluation, and later restore the original.  The returned
    /// value is logically detached: mutating it does not affect
    /// `self`.
    pub fn mobile(&self) -> Mobile {
        self.mobile.clone()
    }

    /// Replace `self.mobile` wholesale with `mobile`.
    ///
    /// The setter half of the [`Self::mobile`] / [`Self::install_mobile`]
    /// pair: completes the swap-in-then-swap-out protocol that lets a
    /// caller move a mobile bundle through a sub-evaluation without
    /// threading it through every call site.  The previous mobile is
    /// dropped; callers wanting to preserve it should snapshot via
    /// [`Self::mobile`] first.  After the call, every read of
    /// `self.mobile.*` observes the freshly installed bundle.
    ///
    /// Restricted to `pub(crate)` so callers outside `core` cannot
    /// silently overwrite the builtin table or handler stack.  Wire-borne
    /// mobiles must go through [`crate::subprocess::install_shell_mobile`],
    /// which preserves the receiver's builtins and splices the wire's
    /// handler frames on top.
    pub(crate) fn install_mobile(&mut self, mobile: Mobile) {
        self.mobile = mobile;
    }

    /// Swap `mobile` in, run `f`, swap back out.
    ///
    /// The combinator form of the [`Self::mobile`] / [`Self::install_mobile`]
    /// pair.  Returns the post-run mobile (which `f` may have mutated
    /// through `&mut Shell`) alongside `f`'s result, so the caller
    /// can inspect or discard the body's mobile mutations explicitly.
    /// At no point during `f` is `self.mobile` the caller's original;
    /// on return, `self.mobile` is byte-for-byte that original (moved
    /// back via [`std::mem::replace`]).  An unwind through `f` would
    /// leave `self.mobile` as the passed-in bundle — acceptable
    /// because ral evaluation does not rely on Rust unwinding for
    /// control flow.
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
    /// lambda — in place on this shell.
    ///
    /// Builds the body's [`Mobile`] from a clone of `self.mobile`, rescoped
    /// to `captured` plus a fresh frame so the body's own `let` bindings
    /// live above the captured closure scope, and hands it to `f` together
    /// with `&mut self`.  `f` installs the mobile for the body's duration
    /// (via [`Self::run_with_mobile`], for both a lambda and a block) and
    /// returns the post-body mobile alongside its result; this routine then
    /// folds the [`ThunkBody`]-specific set back onto the caller's mobile.
    /// The store — `turn`, `session`, `local` — is shared by identity (see
    /// the module doc); this is the single in-place routine block and lambda
    /// elimination meet at.
    ///
    /// `local.repl.pending_chpwd` is bracketed for a [`ThunkBody::Block`] —
    /// a block has no business persisting a REPL notification the parent
    /// would replay — but left to ride the shared `local.repl` for a
    /// [`ThunkBody::Lambda`], where a body `cd` is a real process-state
    /// change: the REPL-notification analogue of the `cwd` fold-back.
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
            // A lambda body enters with a fresh `$?`; a block keeps the
            // caller's, cloned above.
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

    /// Build a fresh [`Shell`] whose lexical environment is a clone
    /// of `captured`.  Other components are defaulted.  Building
    /// block for [`Self::child_of`], [`Self::child_from`], and
    /// [`Self::spawn_thread`]; external callers want one of those,
    /// since a defaulted shell has no inherited grants, env vars, or
    /// call-site location.
    fn from_captured(captured: &Env) -> Self {
        let mut shell = Self::new(crate::io::TerminalState::default());
        shell.mobile.scope = captured.clone();
        shell
    }

    /// Cross-process pipeline-stage child: inherit context state from
    /// `parent` *and* move the read-once bits (pipe stdin, audit trail,
    /// REPL editor context) out of parent for the duration of the child's
    /// life.  Pair with [`Shell::return_to`] to fold the mutations back,
    /// lest the lent state die with the child.
    ///
    /// Unlike a same-thread β-step (which runs in place via
    /// [`Shell::with_thunk_body`]), this builds a *new* `Shell` because the
    /// pipeline stage runs in a separate helper process: its `parent` is a
    /// throwaway reconstructed there ([`crate::child_eval`]), so the loan is
    /// repaid into that throwaway, not a live caller.  The per-call overhead
    /// benchmark also drives it directly to measure the bracket.
    pub fn child_of(captured: &Env, parent: &mut Self) -> Self {
        let mut child = Self::from_captured(captured);
        child.inherit_from(parent);
        child
    }

    /// REPL aside (prompt, hook shell): clone context state from
    /// `parent` without touching its IO / audit / REPL editor
    /// context.  The child is an independent sibling; no flow-back is
    /// needed.  The source cursor (`turn.loc`), the builtin table
    /// (`session.builtins`), and the library doc index
    /// (`session.library_docs`) are copied alongside the context clone so the
    /// aside resolves names, renders positions, and describes itself exactly
    /// as the parent would.
    pub fn child_from(captured: &Env, parent: &Self) -> Self {
        let mut child = Self::from_captured(captured);
        child.mobile.context = parent.mobile.context.clone();
        child.turn.loc = parent.turn.loc.clone();
        child.session.builtins = parent.session.builtins.clone();
        child
            .session
            .library_docs
            .clone_from(&parent.session.library_docs);
        child
            .session
            .guest_jail
            .clone_from(&parent.session.guest_jail);
        child
    }

    /// Fork this shell into an independent child *session* — the primitive a
    /// host uses to spawn a sub-agent that runs its own turns.
    ///
    /// The session-scoped specialisation of [`Self::child_from`]: the child
    /// snapshots this shell's whole lexical `scope` (prelude, libraries, and
    /// every accumulated binding), its dynamic `context` (cwd, env, grants,
    /// handlers), and the installed builtin table, and starts fresh in
    /// everything else — fresh control counters (a new session is not a
    /// continuation of the caller's call stack) and a freshly-defaulted
    /// [`SessionState`](super::SessionState), so it holds no terminal
    /// authority (`TerminalAccess::Denied`, no lease — it is not the
    /// foreground session) and never publishes the process-global signal
    /// slots (it is not the signal-facing session either — its host cancels
    /// it through [`Shell::cancel_handle`]). There is no flow-back: the
    /// child's `cd`, env, and new bindings die with it.
    ///
    /// Routing a host fork through here keeps the "what flows into a child"
    /// decision in the flow matrix rather than at the call site, so a host
    /// cannot silently sever an inheritable datum — the builtin table among
    /// them — by hand-copying only the fields it happened to remember.
    pub fn fork_session(&self) -> Self {
        let mut child = Self::child_from(&self.mobile.scope, self);
        child.session.publishes_signal_slots = false;
        child
    }

    /// Spawn `f` on a fresh OS thread with a cloned child shell.  The
    /// caller supplies `scopes` — the thunk's captured closure scope
    /// for `spawn` / `par`, or the caller's own scope for pipeline
    /// stages — and this shell's [`Context`](super::Context) subtree
    /// is cloned and installed on the new thread.  Per-fork IO setup
    /// lives inside `f`.  The one and only thread-spawn primitive.
    ///
    /// The worker runs under a [`child`](crate::process::DurableRoot::child)
    /// of the **durable root**, not the swappable foreground scope, so a
    /// foreground cancel — a turn timeout or a Ctrl-C on `turn.cancel` —
    /// does not reach it.  Only a
    /// [`RootAbort`](crate::process::CancelCause::RootAbort) on the root,
    /// or a cancel on the worker's own returned scope (via `cancel` /
    /// `race`), stops it.  That returned child scope is stored on the
    /// handle so `cancel` / `race` can stop just this worker.
    ///
    /// `local.workers` — the worker registry — is shared into the new
    /// thread's `Shell` by `Arc` clone, alongside `session.root`,
    /// `session.builtins`, `session.library_docs`, and `session.guest_jail`:
    /// a `spawn` inside `f`'s body registers into the same registry this
    /// shell's own workers
    /// do, rather than a private one of its own.
    pub fn spawn_thread<F, R>(
        &self,
        scopes: Arc<Env>,
        f: F,
    ) -> (std::thread::JoinHandle<R>, crate::process::CancelScope)
    where
        F: FnOnce(&mut Self) -> R + Send + 'static,
        R: Send + 'static,
    {
        let context = self.mobile.context.clone();
        let deferred_lease = self.turn.deferred_lease;
        let worker_cap = self.turn.worker_cap;
        let root = self.session.root.clone();
        let builtins = self.session.builtins.clone();
        let library_docs = self.session.library_docs.clone();
        let guest_jail = self.session.guest_jail.clone();
        let workers = self.local.workers.clone();
        let cancel = root.child();
        let worker_cancel = cancel.as_scope().clone();
        let handle = std::thread::spawn(move || {
            let mut child = Self::from_captured(&scopes);
            child.mobile.context = context;
            child.turn.deferred_lease = deferred_lease;
            child.turn.worker_cap = worker_cap;
            child.turn.cancel = cancel;
            child.session.root = root;
            child.session.builtins = builtins;
            child.session.library_docs = library_docs;
            child.session.guest_jail = guest_jail;
            child.local.workers = workers;
            // Shared, not owned: this worker's shell dropping must not
            // cancel the parent's whole registry.
            child.local.workers_owned = false;
            f(&mut child)
        });
        (handle, worker_cancel)
    }

    /// Propagate runtime state from `parent` into this child shell
    /// for a same-thread thunk body.  Per-substate inherit rules; the
    /// asymmetry with [`Self::return_to`] is the flow matrix.
    pub fn inherit_from(&mut self, parent: &mut Self) {
        self.mobile.context = parent.mobile.context.clone();
        self.mobile.control.inherit_from(&parent.mobile.control);
        self.turn.inherit_from(&mut parent.turn);
        self.local.audit.inherit_from(&mut parent.local.audit);
        self.local.repl.inherit_from(&mut parent.local.repl);
        self.session.builtins = parent.session.builtins.clone();
        self.session.library_docs = parent.session.library_docs.clone();
        self.session.root = parent.session.root.clone();
        self.session.guest_jail = parent.session.guest_jail.clone();
    }

    /// Flow mutations made by a child computation back to `parent`.
    /// Per-substate return rules.  The source cursor (`turn.loc`) and the
    /// `within`-attenuable bits do not flow back; the asymmetry is
    /// the point.
    ///
    /// `cwd` (both halves of the pair) flows back: a `cd` inside a
    /// thunk persists like every other shell.  Threads do not run
    /// `return_to`, so their own `cd`s stay private.
    pub fn return_to(&mut self, parent: &mut Self) {
        self.mobile.control.return_to(&mut parent.mobile.control);
        self.local.audit.return_to(&mut parent.local.audit);
        self.local.repl.return_to(&mut parent.local.repl);
        self.turn.return_to(&mut parent.turn);
        parent.mobile.context.cwd.current = self.mobile.context.cwd.current.take();
        parent.mobile.context.cwd.previous = self.mobile.context.cwd.previous.take();
    }
}

// Unix-only: both tests assert a minted `TerminalLease` is `Some`, but
// `mint_at_startup` returns `None` unconditionally on platforms with no
// `tcsetpgrp` (Windows), so the assertions cannot hold there.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::process::TerminalLease;
    use crate::types::shell::TerminalAccess;

    /// A same-thread lambda body runs in place on the caller's shell, so it
    /// observes the session-owned terminal lease *by identity* — not by a
    /// manifest remembering to copy a witness into a fresh `SessionState`.
    /// A foreground external inside a function / alias / handler body can
    /// therefore take the controlling terminal whenever the turn is
    /// `Leased`, and the lease is plainly still held after the body since it
    /// never moved.
    #[test]
    fn lambda_body_shares_the_session_terminal_lease() {
        let mut shell = Shell::default();
        shell.session.terminal_lease = TerminalLease::mint_at_startup(true);
        shell.turn.terminal_access = TerminalAccess::Leased;
        assert!(
            shell.terminal_lease().is_some(),
            "precondition: the session holds a Leased lease",
        );

        let captured = shell.mobile.scope.clone();
        shell.with_thunk_body(ThunkBody::Lambda, &captured, |shell, mobile| {
            shell.run_with_mobile(mobile, |body| {
                assert!(
                    body.terminal_lease().is_some(),
                    "a Leased lambda body shares the session lease",
                );
            })
        });

        assert!(
            shell.terminal_lease().is_some(),
            "the session still holds the lease after the body — it never moved",
        );
    }

    /// A forked session is not the foreground session, so it holds no terminal
    /// authority — even when forked from a parent that does, and even if the
    /// child's own turn later claims `Leased` access.  `fork_session` builds
    /// the child over a freshly-defaulted `SessionState`, which mints no lease
    /// witness, so a sub-agent can never foreground an external command and
    /// seize the controlling terminal the host's TUI owns.
    #[test]
    fn fork_session_holds_no_terminal_authority() {
        let mut parent = Shell::default();
        parent.session.terminal_lease = TerminalLease::mint_at_startup(true);
        parent.turn.terminal_access = TerminalAccess::Leased;
        assert!(
            parent.terminal_lease().is_some(),
            "precondition: the parent holds a Leased lease",
        );

        let mut child = parent.fork_session();
        child.turn.terminal_access = TerminalAccess::Leased;
        assert!(
            child.terminal_lease().is_none(),
            "a forked session minted no lease witness, so it cannot foreground",
        );
    }
}
