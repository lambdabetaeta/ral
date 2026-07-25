//! CBPV evaluator for `ral`.
//!
//! The run/block evaluation verbs and the tail-absorption seam they share;
//! per-verb contract detail lives on each function's own doc.
//!
//! - [`eval_top_level`] (`pub(crate)`) — runs a top-level run, installing
//!   the post-run [`Mobile`](crate::types::Mobile) on the parent shell on
//!   every outcome.
//! - [`evaluate`] — a bare tail-absorbed run with no mobile contract, for
//!   callers already inside an active session (module loading, prelude
//!   bootstrap, capability profiles, REPL plugin / config loading).
//! - [`eval_block`] (`pub(crate)`) — the block contract: scope-isolated
//!   body, mobile discarded on exit.  External callers reach it through
//!   [`apply`], the crate-private thunk-application verb.
//! - [`absorb_tail`] is the seam every absorption point funnels through to
//!   land an escaping [`TailCall`] and turn a `Raw<Value>` into a
//!   `Settled<Value>`.

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

/// Tail-absorbed run of `comp` against `shell` in place.  Not a run:
/// no mobile install or discard, no scope frame, no transport dispatch.
///
/// Evaluated under a non-trivial continuation ([`Tail::No`]): the caller
/// already has obligations beyond `comp` (it threads the result on),
/// and `absorb_tail` lands any terminal tail call locally regardless.
///
/// For callers already inside an active session — module loading,
/// prelude bootstrap, capability profiles, REPL plugin / config files.
/// Wrapping them in a run boundary would round-trip a mobile they
/// never wanted snapshotted.
///
/// # Errors
/// Returns `Err` if evaluating `comp` raises a `Break` — a recoverable
/// runtime error (`Break::Error`) or a non-local escape (`Break::Escape`,
/// e.g. `exit`).
pub fn evaluate(comp: &Arc<Comp>, shell: &mut Shell) -> Settled<Value> {
    debug_assert!(
        !shell.mobile.context.grants.is_empty(),
        "grants must be non-empty; Shell::new pre-pushes root"
    );
    absorb_tail(comp::eval_comp(comp, shell, Tail::No), shell)
}

// ── Boundary verbs ───────────────────────────────────────────────────────
//
// Both verbs evaluate the body **in process**: a `grant` is a dynamic
// effect scope, not a process boundary, so nested grants compose by
// intersection over authority in the evaluator's dynamic context.  OS
// confinement is a separate, per-child locus — the launcher in
// `build_command` (crate::runtime::command::process) enters the OS sandbox
// for an admitted external or bundled-as-image child under an active
// projection, so a `grant [net:false] { <pure ral> }` fails closed only
// when a child is actually spawned, not at body entry.

/// Run `comp` as a top-level run.  Installs the post-run [`Mobile`]
/// on `shell` for every outcome — Ok, Error, Exit — so a `let`
/// followed by a failing command still leaves the binding visible to
/// the next run.
///
/// The run's program is its sole computation, evaluated under a
/// trivial continuation ([`Tail::Yes`]): its value is handed straight
/// back to the [`Shell::run`](crate::Shell::run) door that
/// drove it, which relays it to the host.  The run's mobile is swapped
/// in for its duration, so any terminal tail call is absorbed under it.
pub(crate) fn eval_top_level(comp: &Arc<Comp>, shell: &mut Shell) -> Settled<Value> {
    let mobile = shell.mobile();
    let (post, outcome) = shell.run_with_mobile(mobile, |shell| {
        absorb_tail(comp::eval_comp(comp, shell, Tail::Yes), shell)
    });
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
/// `Value::Block` carries.  [`Shell::with_thunk_body`] runs the body in
/// place: it swaps in a mobile rescoped to `captured` plus a fresh frame
/// for the body's own `let` bindings, brackets the parent's
/// `repl.pending_chpwd`, and — as a [`ThunkBody::Block`] — folds only
/// `last_status` back, discarding the body's `cd`.
///
/// `tail` forwards the block's own tail position to its body: a block
/// applied in tail position (the trampoline's final argument) grants
/// [`Tail::Yes`] so the body's final call trampolines; a forced block
/// or a pipeline value-edge force passes [`Tail::No`].  The body's
/// mobile is swapped in for the run, absorbing any terminal tail call
/// under it, so the grant only chooses whether that absorption
/// trampolines.
///
/// `pub(crate)`: external callers reach the block contract through
/// [`apply`] on a `Value::Block`.
pub(crate) fn eval_block(
    body: &Arc<Comp>,
    captured: &Arc<Env>,
    tail: Tail,
    shell: &mut Shell,
) -> Settled<Value> {
    shell.with_thunk_body(ThunkBody::Block, captured, |shell, mobile| {
        shell.run_with_mobile(mobile, |shell| {
            absorb_tail(comp::eval_comp(body, shell, tail), shell)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `let` followed by `exit` must still leave the binding
    /// installed for the next run.
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
        let _ = eval_block(&body, &captured, Tail::No, &mut shell);
        assert!(
            shell.mobile.scope.get("leak_block").is_none(),
            "block boundary must discard `let` bindings"
        );
    }
}
