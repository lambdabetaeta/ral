//! Local-vs-confined transport routing for the boundary verbs.
//!
//! The boundary verbs in [`crate::evaluator`] decide *what survives* a
//! run (mobile install vs discard).  This module decides *where* it
//! actually runs — same process, or an OS-sandboxed child reached
//! over IPC (Seatbelt on macOS, bwrap on Linux, AppContainer on
//! Windows).  The two concerns are orthogonal; every cell of the 2×2
//! (top-level / block × local / confined) is reachable in production.
//!
//! [`command_call`] picks transport from the shell's current
//! [`SandboxProjection`](crate::types::SandboxProjection).  The rule
//! fails closed:
//!
//! 1. An **authenticated** confinement marker (this process is genuinely
//!    a confined ral child), or no active projection → run **local**.
//!    The marker is tested first: when it holds, the projection is moot,
//!    and computing it would needlessly resolve the exec policy through
//!    `$PATH` on every boundary verb the child evaluates.  Re-entering OS
//!    sandboxing from inside one is redundant under bwrap and outright
//!    impossible under Seatbelt (one-shot per process; a second
//!    `sandbox_init_with_parameters` returns EPERM).  The marker is
//!    authenticated against a per-re-exec capability token
//!    ([`crate::sandbox`] `marker`), *not* the mere presence of the
//!    inheritable `RAL_SANDBOX_ACTIVE` env var — a bare or forged marker
//!    from an arbitrary parent does not authenticate, so it cannot
//!    suppress confinement (review findings S1, S2).
//! 2. Active projection, backend
//!    [`Unavailable`](crate::sandbox::ConfinedAvailability::Unavailable)
//!    → error.  Falling through to local would silently weaken the
//!    user's restriction.
//! 3. Active projection, backend `Ready` → run **confined**.
//!
//! `spawn` / `watch` workers do not enter this dispatch — the worker
//! thread's lifecycle is the block boundary, and an OS sandbox on the
//! parent already wraps the thread.

use std::sync::Arc;

use crate::ir::Comp;
use crate::sandbox::{ConfinedAvailability, confined_availability};
use crate::types::{Break, Mobile, Settled, Shell, Tail, Value};

use crate::evaluator::absorb_tail;
use crate::evaluator::comp::eval_comp;

/// Choose local or confined transport and run `body` against `mobile`.
/// Returns the post-run mobile and the body's outcome.  See module
/// docs for the fail-closed rule.  Audit fragments from a confined
/// run are merged into `shell.local.audit` inside the confined runner;
/// nothing about them bubbles up.
///
/// `tail` is the body's tail position, forwarded from the boundary verb
/// ([`eval_top_level`](crate::evaluator::eval_top_level) /
/// [`eval_block`](crate::evaluator::eval_block)).  Only the local path
/// observes it: a confined body runs in a child process that absorbs its
/// own terminal tail call, so the wire carries no tail-ness.
pub(crate) fn dispatch(
    body: &Arc<Comp>,
    mobile: Mobile,
    tail: Tail,
    shell: &mut Shell,
) -> (Mobile, Settled<Value>) {
    // Test the marker first so an already-confined child need not resolve
    // the exec policy through `$PATH`; then bind the projection once,
    // since computing it is what does that resolution.
    if crate::sandbox::marker_authenticated() {
        return run_local(body, mobile, tail, shell);
    }
    let Some(projection) = shell.sandbox_projection() else {
        return run_local(body, mobile, tail, shell);
    };
    // Hand the input mobile back so the contract layer has a well-defined
    // value to install / discard, with `last_status` overwritten so the
    // parent sees the refusal rather than the previous turn's status.
    let fail_closed = |mobile: Mobile, shell: &mut Shell, reason: &str| {
        let mut mobile = mobile;
        mobile.control.last_status = 1;
        (
            mobile,
            Err(Break::Error(shell.err(
                format!("sandbox confinement unavailable: {reason}"),
                1,
            ))),
        )
    };
    match confined_availability() {
        ConfinedAvailability::Ready => match crate::sandbox::projection_enforceable(&projection) {
            Ok(()) => run_confined(body, mobile, shell),
            Err(reason) => fail_closed(mobile, shell, reason),
        },
        ConfinedAvailability::Unavailable(reason) => fail_closed(mobile, shell, reason),
    }
}

/// Swap `mobile` into `shell` for the body, then swap back.
///
/// Tail absorption happens *inside* the swap: the trampoline iteration
/// that lands a tail callee must run under the body's mobile, since
/// the tail call lives in the same frame as the caller.
fn run_local(
    body: &Arc<Comp>,
    mobile: Mobile,
    tail: Tail,
    shell: &mut Shell,
) -> (Mobile, Settled<Value>) {
    shell.run_with_mobile(mobile, |shell| {
        absorb_tail(eval_comp(body, shell, tail), shell)
    })
}

/// Run the body in an OS-sandboxed child over IPC.  Transport failures
/// (spawn, IPC decode, etc.) come back as `Err`; we fold them into a
/// pair carrying the parent's current mobile (with `last_status`
/// overwritten to 1) so the contract layer has a well-defined value
/// to install / discard.
fn run_confined(body: &Arc<Comp>, mobile: Mobile, shell: &mut Shell) -> (Mobile, Settled<Value>) {
    match crate::sandbox::run_confined(body, mobile, shell) {
        Ok(pair) => pair,
        Err(err) => {
            let mut mobile = shell.mobile();
            mobile.control.last_status = 1;
            (mobile, Err(err))
        }
    }
}
