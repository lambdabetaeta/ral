//! CBPV evaluation verbs: the top-level run, the bare in-session run, and
//! the block.  Each funnels its result through [`absorb_tail`], the one seam
//! that lands an escaping [`TailCall`], so `Tail` never leaves this module
//! tree.

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
use crate::types::{Control, Env, Mooring, Settled, Shell, Tail, TailCall, ThunkBody, Value};
use std::sync::Arc;

pub(crate) use capture::with_audit_capture;
pub use capture::with_capture;
pub(crate) use trampoline::apply;

// ── Tail absorption ──────────────────────────────────────────────────────

/// Land an escaping [`TailCall`] through the trampoline; pass `Break`
/// verbatim.  The one `Raw<Value>` → `Settled<Value>` seam: the verbs here,
/// thread-worker roots, `child_eval`'s cross-process stage, redirect bodies.
pub(crate) fn absorb_tail(
    raw: crate::types::Raw<Value>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    match raw {
        Ok(v) => Ok(v),
        Err(Control::Tail(TailCall { callee, args })) => apply(callee, args, mooring, shell),
        Err(Control::Break(b)) => Err(b),
    }
}

/// Tail-absorbed run of `comp` in place — no mobile install or discard, no
/// scope frame, no transport dispatch.  For callers already inside a session
/// (prelude bootstrap, module loading, REPL prompt and config), where a run
/// boundary would round-trip a mobile they never wanted snapshotted.
/// [`Tail::No`], since the caller has obligations beyond `comp`.
///
/// # Errors
/// A `Break` from `comp` — a recoverable error, or an escape such as `exit`.
pub fn evaluate(comp: &Arc<Comp>, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    debug_assert!(
        !shell.mobile.context.grants.is_empty(),
        "grants must be non-empty; Shell::new pre-pushes root"
    );
    absorb_tail(
        comp::eval_comp(comp, mooring, shell, Tail::No),
        mooring,
        shell,
    )
}

// ── Boundary verbs ───────────────────────────────────────────────────────
//
// Both evaluate the body in this process: a `grant` is a dynamic effect
// scope, not a process boundary, so OS confinement happens per child in
// `build_command` (`runtime::command::process`), never at body entry.

/// Run `comp` as a top-level run, installing the post-run
/// [`Mobile`](crate::types::Mobile) for every outcome — Ok, Error, Exit —
/// so a `let` before a failing command still binds for the next run.
///
/// The program is the run's sole computation, hence [`Tail::Yes`]; its value
/// goes straight back to the [`Shell::run`](crate::Shell::run) door.  The
/// run's mobile is swapped in for the duration, so a terminal tail call is
/// absorbed under it.
pub(crate) fn eval_top_level(
    comp: &Arc<Comp>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let mobile = shell.mobile();
    let (post, outcome) = shell.run_with_mobile(mobile, |shell| {
        absorb_tail(
            comp::eval_comp(comp, mooring, shell, Tail::Yes),
            mooring,
            shell,
        )
    });
    shell.install_mobile(post);
    outcome
}

/// Run a thunk body as a block: scope-isolated, mobile discarded on exit, so
/// `let`, `cd`, module loads, plugin registrations, and env-var changes do
/// not propagate.  Only `last_status` and audit nodes, which the body posts
/// straight to the shared trail, cross the boundary.
///
/// `captured` is the lexical environment a `Value::Block` carries;
/// [`Shell::with_thunk_body`] rescopes the body's mobile to it plus a fresh
/// frame and, as a [`ThunkBody::Block`], folds only `last_status` back.
///
/// `tail` is the block's own tail position, granted [`Tail::Yes`] only by the
/// trampoline's final argument.  The body's mobile is installed for the
/// absorption either way; tail-ness only chooses whether it trampolines.
///
/// Outside the crate the block contract is reached through [`apply`].
pub(crate) fn eval_block(
    body: &Arc<Comp>,
    captured: &Arc<Env>,
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    shell.with_thunk_body(ThunkBody::Block, captured, |shell, mobile| {
        shell.run_with_mobile(mobile, |shell| {
            absorb_tail(comp::eval_comp(body, mooring, shell, tail), mooring, shell)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_persists_let_on_error() {
        let source = "let persist_top = 41; exit 7";
        let comp = Arc::new(crate::compile(source).expect("compile"));
        let mut shell = Shell::default();
        let _ = eval_top_level(&comp, &Mooring::adrift(), &mut shell);
        assert!(
            shell.mobile.scope.get("persist_top").is_some(),
            "top-level mobile must persist `let` bindings even on Exit"
        );
    }

    #[test]
    fn block_discards_let() {
        let source = "let leak_block = 1";
        let body = Arc::new(crate::compile(source).expect("compile"));
        let mut shell = Shell::default();
        let captured = shell.snapshot();
        let _ = eval_block(&body, &captured, Tail::No, &Mooring::adrift(), &mut shell);
        assert!(
            shell.mobile.scope.get("leak_block").is_none(),
            "block boundary must discard `let` bindings"
        );
    }
}
