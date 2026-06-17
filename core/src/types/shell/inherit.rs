//! State transfer between parent and child interpreter states.
//!
//! Three modes carve up how a child [`Shell`] is derived from its
//! parent:
//!
//! - **Thunk body** (same thread): [`Shell::child_of`] clones the
//!   parent's [`Context`](super::Context), moves the read-once local
//!   bits (pipe stdin, audit trail, REPL editor context) out of
//!   parent, and lends them to the child.
//!   [`Shell::with_child`] is the combinator that pairs this with
//!   [`Shell::return_to`] so child mutations flow back.
//! - **Spawned thread** (`spawn`, `par`, pipeline stage):
//!   [`Shell::spawn_thread`] snapshots the parent's [`Context`] and
//!   ships it to a fresh thread that owns its own IO; nothing flows
//!   back.
//! - **REPL aside** (prompt, hook): [`Shell::child_from`] clones the
//!   parent's [`Context`] without touching its local machinery; the
//!   child is an independent sibling with no flow-back.
//!
//! Plus a fourth, finer-grained variant that swaps state on the
//! *same* shell rather than building a new one:
//!
//! - **Block body**: [`Shell::with_block`] installs a fresh mobile
//!   built from the captured closure scope (a freshly-pushed frame on
//!   top of `captured`), runs `f` with that mobile in hand, and
//!   restores the parent's REPL scratch on return.  Local machinery
//!   is shared with the parent, so the body's writes and audit nodes
//!   land where every other body's would.
//!
//! [`Shell::inherit_from`] and [`Shell::return_to`] are the
//! per-substate manifests the first three modes lean on: `mobile.context`
//! clones whole; `mobile.control`, the [`TurnState`](super::TurnState)
//! substates (via [`TurnState::inherit_from`](super::TurnState::inherit_from)
//! / [`return_to`](super::TurnState::return_to)), `local.audit`, and
//! `local.repl` each carry their own inherit / return rule; `session.builtins`
//! and `session.root` are shared so dispatch and the cancel root reach the
//! child.  The asymmetry between the two manifests is the flow matrix — the
//! source cursor (`turn.loc`) and the `within`-attenuable bits do not flow
//! back, but `context.cwd` does.

use super::{Mobile, Shell};
use crate::types::Env;
use std::sync::Arc;

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
        f: impl FnOnce(&mut Shell) -> R,
    ) -> (Mobile, R) {
        let saved = std::mem::replace(&mut self.mobile, mobile);
        let result = f(self);
        let post = std::mem::replace(&mut self.mobile, saved);
        (post, result)
    }

    /// Run `f` over a block-body [`Mobile`] derived from this shell
    /// and `captured`.
    ///
    /// Builds a mobile from a clone of `self.mobile`, replaces its
    /// lexical scope with `captured`, and pushes a fresh frame on top
    /// — so the body's own `let` bindings live above the captured
    /// closure scope.  The mobile is handed to `f`; the caller passes
    /// it to `dispatch`, which threads its own swap-in / swap-out
    /// around the body.  Local machinery (IO, audit, cancel) stays on
    /// the parent — the block shares it rather than carving out its
    /// own — but `local.repl.pending_chpwd` is saved and restored
    /// here so the block has no business persisting a REPL-local
    /// notification that the parent would replay on its next prompt.
    pub fn with_block<R>(&mut self, captured: &Env, f: impl FnOnce(&mut Self, Mobile) -> R) -> R {
        let mut mobile = self.mobile.clone();
        mobile.scope = captured.clone();
        mobile.scope.push_scope();
        let saved_pending_chpwd = self.local.repl.pending_chpwd.take();
        let result = f(self, mobile);
        self.local.repl.pending_chpwd = saved_pending_chpwd;
        result
    }

    /// Build a fresh [`Shell`] whose lexical environment is a clone
    /// of `captured`.  Other components are defaulted.  Building
    /// block for [`Self::child_of`], [`Self::child_from`], and
    /// [`Self::spawn_thread`]; external callers want one of those,
    /// since a defaulted shell has no inherited grants, env vars, or
    /// call-site location.
    fn from_captured(captured: &Env) -> Self {
        let mut shell = Self::new(Default::default());
        shell.mobile.scope = captured.clone();
        shell
    }

    /// Thunk body: inherit context state from `parent` *and* move the
    /// read-once same-thread bits (pipe stdin, audit trail, REPL
    /// editor context) out of parent for the duration of the child's
    /// life.  Pair with [`Shell::return_to`] to fold the mutations
    /// back, lest the lent state die with the child.
    ///
    /// [`Shell::with_child`] is the paired lend-and-return entry and
    /// the form same-thread callers want: it brackets `child_of` with
    /// `return_to` so the loan is always repaid.  The open form here is
    /// for callers with no live parent to repay — the cross-process
    /// pipeline-stage child in [`crate::child_eval`], whose `parent` is
    /// a throwaway in the helper process, and the per-call overhead
    /// benchmark that measures the bracket directly.
    pub fn child_of(captured: &Env, parent: &mut Shell) -> Self {
        let mut child = Self::from_captured(captured);
        child.inherit_from(parent);
        child
    }

    /// REPL aside (prompt, hook shell): clone context state from
    /// `parent` without touching its IO / audit / REPL editor
    /// context.  The child is an independent sibling; no flow-back is
    /// needed.  The source cursor and the builtin table — no longer
    /// part of `context` — are copied alongside so the aside resolves
    /// names and renders positions exactly as the parent would.
    pub fn child_from(captured: &Env, parent: &Shell) -> Self {
        let mut child = Self::from_captured(captured);
        child.mobile.context = parent.mobile.context.clone();
        child.turn.loc = parent.turn.loc.clone();
        child.session.builtins = parent.session.builtins.clone();
        child
    }

    /// Run `f` in a child shell derived from `captured` and this
    /// shell (via [`Self::child_of`]), then fold side-effects back
    /// via [`Self::return_to`].  The canonical same-thread thunk
    /// call.
    pub fn with_child<R>(&mut self, captured: &Env, f: impl FnOnce(&mut Shell) -> R) -> R {
        let mut child = Shell::child_of(captured, self);
        let result = f(&mut child);
        child.return_to(self);
        result
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
    pub fn spawn_thread<F, R>(
        &self,
        scopes: Arc<Env>,
        f: F,
    ) -> (std::thread::JoinHandle<R>, crate::process::CancelScope)
    where
        F: FnOnce(&mut Shell) -> R + Send + 'static,
        R: Send + 'static,
    {
        let context = self.mobile.context.clone();
        let surface = self.turn.surface.clone();
        let detached_ceiling = self.turn.detached_ceiling;
        let root = self.session.root.clone();
        let builtins = self.session.builtins.clone();
        let cancel = root.child();
        let worker_cancel = cancel.as_scope().clone();
        let handle = std::thread::spawn(move || {
            let mut child = Self::from_captured(&scopes);
            child.mobile.context = context;
            child.turn.surface = surface;
            child.turn.detached_ceiling = detached_ceiling;
            child.turn.cancel = cancel;
            child.session.root = root;
            child.session.builtins = builtins;
            f(&mut child)
        });
        (handle, worker_cancel)
    }

    /// Propagate runtime state from `parent` into this child shell
    /// for a same-thread thunk body.  Per-substate inherit rules; the
    /// asymmetry with [`Self::return_to`] is the flow matrix.
    pub fn inherit_from(&mut self, parent: &mut Shell) {
        self.mobile.context = parent.mobile.context.clone();
        self.mobile.control.inherit_from(&parent.mobile.control);
        self.turn.inherit_from(&mut parent.turn);
        self.local.audit.inherit_from(&mut parent.local.audit);
        self.local.repl.inherit_from(&mut parent.local.repl);
        self.session.builtins = parent.session.builtins.clone();
        self.session.root = parent.session.root.clone();
    }

    /// Flow mutations made by a child computation back to `parent`.
    /// Per-substate return rules.  The source cursor (`turn.loc`) and the
    /// `within`-attenuable bits do not flow back; the asymmetry is
    /// the point.
    ///
    /// `cwd` (both halves of the pair) flows back: a `cd` inside a
    /// thunk persists like every other shell.  Threads do not run
    /// `return_to`, so their own `cd`s stay private.
    pub fn return_to(&mut self, parent: &mut Shell) {
        self.mobile.control.return_to(&mut parent.mobile.control);
        self.local.audit.return_to(&mut parent.local.audit);
        self.local.repl.return_to(&mut parent.local.repl);
        self.turn.return_to(&mut parent.turn);
        parent.mobile.context.cwd.current = self.mobile.context.cwd.current.take();
        parent.mobile.context.cwd.previous = self.mobile.context.cwd.previous.take();
    }
}
