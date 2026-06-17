//! Transport routing for the boundary verbs.
//!
//! The boundary verbs in [`crate::evaluator`] decide *what survives* a
//! run (mobile install vs discard).  This module decides *where* the
//! body runs — and the answer is now always **in process**.  A `grant`
//! is a dynamic effect scope, not a process boundary: nested grants
//! compose by intersection over authority, an algebra that belongs to
//! the evaluator's dynamic context.  The grant body therefore always
//! evaluates locally.
//!
//! Confinement happens at a different locus.  Effects split two ways:
//!
//! - **RAL-owned effects** (structured filesystem primitives, redirects,
//!   editor access, shell-state mutation, the top-level exec judgment)
//!   are decided in process by `capability::check_*` — fs operations in
//!   particular by [`check_fs_op`](crate::capability::check_fs_op) on the
//!   canonical path before the syscall.
//! - **Child-owned effects** are kernel-backed.  When an admitted
//!   external or bundled-as-image command is spawned under a restrictive
//!   projection, the per-command launcher in
//!   [`build_command`](crate::runtime::command::process) enters the OS
//!   sandbox (Seatbelt on macOS, bwrap on Linux, restricted token on
//!   Windows) for that one child.  That gate fires when the process is
//!   not already confined and a projection is active.
//!
//! Net/fs fail-closed for a grant therefore happens at **external
//! dispatch** (`build_command` → `projection_enforceable` /
//! `sandboxed_command`), not at grant-body entry.  A
//! `grant [net:false] { <pure-ral code, no external> }` no longer fails
//! closed on an unenforceable backend, because there is no child to
//! confine; it fails closed only when a child is actually spawned.  This
//! is intended.
//!
//! `spawn` / `watch` workers do not enter this dispatch — the worker
//! thread's lifecycle is the block boundary.

use std::sync::Arc;

use crate::ir::Comp;
use crate::types::{Mobile, Settled, Shell, Tail, Value};

use crate::evaluator::absorb_tail;
use crate::evaluator::comp::eval_comp;

/// Run `body` against `mobile` in process and return the post-run mobile
/// and the body's outcome.
///
/// `tail` is the body's tail position, forwarded from the boundary verb
/// ([`eval_top_level`](crate::evaluator::eval_top_level) /
/// [`eval_block`](crate::evaluator::eval_block)) and observed by the
/// local run.
pub(crate) fn dispatch(
    body: &Arc<Comp>,
    mobile: Mobile,
    tail: Tail,
    shell: &mut Shell,
) -> (Mobile, Settled<Value>) {
    run_local(body, mobile, tail, shell)
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
