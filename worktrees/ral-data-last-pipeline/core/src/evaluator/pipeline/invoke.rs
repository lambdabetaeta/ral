//! Stage invocation: the pipeline runtime's view of a single internal stage.
//!
//! Each stage is invoked with an optional upstream value.  The upstream is
//! the data-last argument: appended to whatever args the stage's call form
//! already carries.  This is the *only* place that knows about cross-stage
//! value flow — `eval_call_parts` is now ignorant of pipelines.
//!
//! The decomposition reads the stage's syntactic shape:
//!
//!   * `Exec`  — name dispatch via `dispatch::dispatch_by_name`.
//!   * `App`   — eval head, eval args, append upstream, `eval_app`.
//!   * `Return(Thunk)` / `Force(Val::Variable(_))` / `Force(Val::Thunk(_))` —
//!     a thunk-shaped stage: callee is the thunk value itself, applied to
//!     `[upstream]` via the trampoline.  This is what closes the
//!     block-as-stage case (`5 | { |x| echo $x }`).
//!   * Anything else — evaluate as a plain comp (no upstream visible to it),
//!     then trampoline-apply upstream to the result.  The trampoline's
//!     existing arity errors do the right thing for `5 | 6`,
//!     `5 | { echo hi }`, etc.

use crate::ir::{Comp, CompKind, Val};
use crate::types::*;

use super::super::{dispatch, eval_comp, eval_val, trampoline};

/// Push-with-clone for the upstream value — keeps the call sites tidy.
fn push_upstream(args: &mut Vec<Value>, upstream: Option<Value>) {
    if let Some(v) = upstream {
        args.push(v);
    }
}

/// Invoke `stage` with an optional `upstream` value.
///
/// The upstream is appended as the final positional argument of whatever
/// call the stage represents — preserving the data-last reading promised
/// by SPEC.md §4.2.  When the stage is not a recognisable call shape, the
/// fallback is to evaluate the stage and apply its result to the upstream
/// via the trampoline.
pub(super) fn invoke(
    stage: &Comp,
    upstream: Option<Value>,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    match &stage.kind {
        CompKind::Exec {
            name,
            args,
            redirects,
            external_only,
        } => {
            let (mut arg_vals, redir_eval) = dispatch::eval_subcall(shell, |shell| {
                dispatch::eval_call_parts(args, redirects, shell)
            })?;
            push_upstream(&mut arg_vals, upstream);
            dispatch::dispatch_by_name(name, &arg_vals, &redir_eval, *external_only, shell)
        }

        CompKind::App {
            head,
            args,
            redirects,
        } => {
            let (head_val, mut arg_vals, redir_eval) = dispatch::eval_subcall(shell, |shell| {
                let head_val = eval_comp(head, shell)?;
                let (arg_vals, redir_eval) = dispatch::eval_call_parts(args, redirects, shell)?;
                Ok((head_val, arg_vals, redir_eval))
            })?;
            push_upstream(&mut arg_vals, upstream);
            dispatch::eval_app(&head_val, &arg_vals, &redir_eval, shell)
        }

        // Thunk-shaped stages: the user wrote a callable in stage position.
        // Evaluate to a value, then either return it (no upstream) or apply
        // it to upstream via the trampoline.  Same machinery as `eval_app`
        // for the no-redirects, no-pre-args case.
        CompKind::Return(v @ Val::Thunk(_)) | CompKind::Force(v @ Val::Variable(_)) => {
            let callee = eval_val(v, shell)?;
            apply_to_upstream(callee, upstream, shell)
        }

        // Everything else: opaque comp.  Evaluate it (it never sees the
        // upstream), then apply the result to upstream if present.
        _ => {
            let result = eval_comp(stage, shell)?;
            apply_to_upstream(result, upstream, shell)
        }
    }
}

/// Apply `callee` to `[upstream]` via the trampoline, or return `callee`
/// unchanged when there's no upstream.
fn apply_to_upstream(
    callee: Value,
    upstream: Option<Value>,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    match upstream {
        None => Ok(callee),
        Some(v) => trampoline::trampoline(callee, vec![v], shell),
    }
}
