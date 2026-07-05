//! CBPV evaluator for `ral`.
//!
//! Two verbs reach outside the crate; [`apply`] is crate-private.
//!
//! - [`eval_top_level`] (`pub(crate)`) — the turn-evaluation verb a
//!   tool call, a REPL turn, or a script line settles through.  Hosts do
//!   not call it directly; they enter through the framed
//!   [`Shell::run_source_turn`](crate::Shell::run_source_turn) door, which drives it.
//!   The post-run [`crate::types::Mobile`] is *installed* on the parent shell on
//!   every outcome (Ok / Error / Exit); a top-level turn is a resume
//!   point, so every persistable state change must survive.
//! - [`evaluate`] — bare tail-absorbed run with no mobile contract.
//!   For callers already inside an active session: module loading,
//!   prelude bootstrap, capability profiles, REPL plugin / config
//!   loading.
//!
//! [`apply`] (`pub(crate)`) reduces a thunk applied to arguments,
//! absorbing tail signals through the trampoline.  Blocks reach their
//! contract through it: the trampoline pattern-matches `Value::Thunk`
//! and delegates to the internal [`eval_block`].  It is reached from
//! outside the evaluator only through the host turn doors
//! ([`crate::Shell::run_hook`]) and the in-frame builtin wrapper
//! [`crate::builtins::apply`], so a host cannot start an unframed
//! reduction; the in-crate callers ([`crate::runtime`]) already hold a
//! turn frame.
//!
//! [`eval_block`] (`pub(crate)`) is the block contract: scope-isolated
//! body, mobile discarded on exit, local scratch reset, audit fragment
//! merged into the parent trail.  Invoked by the trampoline's
//! `Value::Thunk` arm, by [`comp::step_force`](crate::evaluator::comp::step_force), and by the pipeline's
//! stream-iteration tail dispatch.  `spawn` / `watch` workers bypass
//! it — the worker thread's lifecycle satisfies block discipline on
//! its own.
//!
//! Where the body actually runs (in-process vs OS-sandboxed child over
//! IPC) is orthogonal to the contract and is delegated to
//! [`crate::runtime::transport::dispatch`].

pub mod audit;
pub(crate) mod call;
pub(crate) mod capture;
pub(crate) mod case;
pub(crate) mod comp;
pub mod expr;
pub(crate) mod pattern;
pub(crate) mod redirect;
pub(crate) mod scope;
pub(crate) mod trampoline;
pub(crate) mod val;

use crate::ir::Comp;
use crate::types::{Control, Env, Settled, Shell, Tail, TailCall, ThunkBody, Value};
use std::sync::Arc;

pub(crate) use capture::with_audit_capture;
pub use capture::with_capture;
pub(crate) use trampoline::apply;

// ── Tail absorption ──────────────────────────────────────────────────────

/// Land any escaping [`TailCall`] through the trampoline; pass `Break`
/// through verbatim.  The seam every absorption point funnels through
/// to turn a `Raw<Value>` into a `Settled<Value>` — boundary verbs,
/// thread-worker roots, IPC seams, pipeline stage helpers.
pub(crate) fn absorb_tail(raw: crate::types::Raw<Value>, shell: &mut Shell) -> Settled<Value> {
    match raw {
        Ok(v) => Ok(v),
        Err(Control::Tail(TailCall { callee, args })) => apply(callee, args, shell),
        Err(Control::Break(b)) => Err(b),
    }
}

/// Tail-absorbed run of `comp` against `shell` in place.  Not a turn:
/// no mobile install or discard, no scope frame, no transport dispatch.
///
/// Evaluated under a non-trivial continuation ([`Tail::No`]): the caller
/// already has obligations beyond `comp` (it threads the result on),
/// and `absorb_tail` lands any terminal tail call locally regardless.
///
/// For callers already inside an active session — module loading,
/// prelude bootstrap, capability profiles, REPL plugin / config files.
/// Wrapping them in a turn boundary would round-trip a mobile they
/// never wanted snapshotted.
pub fn evaluate(comp: &Arc<Comp>, shell: &mut Shell) -> Settled<Value> {
    debug_assert!(
        !shell.mobile.context.grants.is_empty(),
        "grants must be non-empty; Shell::new pre-pushes root"
    );
    absorb_tail(comp::eval_comp(comp, shell, Tail::No), shell)
}

// ── Boundary verbs ───────────────────────────────────────────────────────

/// Run `comp` as a top-level turn.  Installs the post-run [`Mobile`]
/// on `shell` for every outcome — Ok, Error, Exit — so a `let`
/// followed by a failing command still leaves the binding visible to
/// the next turn.
///
/// The turn's program is its sole computation, evaluated under a
/// trivial continuation ([`Tail::Yes`]): its value is handed straight
/// back to the [`Shell::run_source_turn`](crate::Shell::run_source_turn) door that
/// drove it, which relays it to the host.  `dispatch` absorbs any
/// terminal tail call inside the swapped-in mobile.
pub(crate) fn eval_top_level(comp: &Arc<Comp>, shell: &mut Shell) -> Settled<Value> {
    let mobile = shell.mobile();
    let (post, outcome) = crate::runtime::transport::dispatch(comp, mobile, Tail::Yes, shell);
    shell.install_mobile(post);
    outcome
}

/// Run a thunk body as a block: scope-isolated, mobile discarded on
/// exit.  `let`, `cd`, module loads, plugin registrations, env-var
/// changes — none propagate.  Only `last_status` (folded onto
/// `shell.mobile`) and audit nodes (which the body posts straight to the
/// shared trail) cross the boundary.
///
/// `captured` is the closure's lexical environment — the snapshot a
/// `Value::Thunk` carries.  [`Shell::with_thunk_body`] runs the body in
/// place: it swaps in a mobile rescoped to `captured` plus a fresh frame
/// for the body's own `let` bindings, brackets the parent's
/// `repl.pending_chpwd`, and — as a [`ThunkBody::Block`] — folds only
/// `last_status` back, discarding the body's `cd`.
///
/// `tail` forwards the block's own tail position to its body: a block
/// applied in tail position (the trampoline's final argument) grants
/// [`Tail::Yes`] so the body's final call trampolines; a forced block
/// or a pipeline value-edge force passes [`Tail::No`].  `dispatch`
/// absorbs any terminal tail call inside the swapped-in mobile, so the
/// grant only chooses whether that absorption trampolines.
///
/// `pub(crate)`: external callers reach the block contract through
/// [`apply`] on a `Value::Thunk`.
pub(crate) fn eval_block(
    body: &Arc<Comp>,
    captured: Arc<Env>,
    tail: Tail,
    shell: &mut Shell,
) -> Settled<Value> {
    shell.with_thunk_body(ThunkBody::Block, &captured, |shell, mobile| {
        crate::runtime::transport::dispatch(body, mobile, tail, shell)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `let` followed by `exit` must still leave the binding
    /// installed for the next turn.
    #[test]
    fn top_level_persists_let_on_error() {
        let source = "let persist_top = 41; exit 7";
        let comp = Arc::new(crate::compile(source).expect("compile"));
        let mut shell = Shell::default();
        let _ = eval_top_level(&comp, &mut shell);
        assert!(
            shell.mobile.scope.get("persist_top").is_some(),
            "top-level mobile must persist `let` bindings even on Exit"
        );
    }

    /// A `let` inside a block must not be visible after.
    #[test]
    fn block_discards_let() {
        let source = "let leak_block = 1";
        let body = Arc::new(crate::compile(source).expect("compile"));
        let mut shell = Shell::default();
        let captured = shell.snapshot();
        let _ = eval_block(&body, captured, Tail::No, &mut shell);
        assert!(
            shell.mobile.scope.get("leak_block").is_none(),
            "block boundary must discard `let` bindings"
        );
    }
}
