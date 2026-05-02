//! Single locus for invoking call-shaped computations.
//!
//! Every `App` / `Exec` / `Builtin` evaluation funnels through `invoke`,
//! and so does every internal pipeline stage.  The only difference is the
//! optional `upstream` value: the pipeline runtime supplies one (data-last,
//! appended after the explicit args); the evaluator's normal call arms
//! supply `None`.  That single parameter is the entire vocabulary of cross-
//! stage value flow — there is no `value_in` side channel anywhere.
//!
//! Decomposition by stage shape:
//!
//!   * `Exec`    — name dispatch via [`dispatch::dispatch_by_name`].
//!   * `App`     — eval head, eval args, append upstream, [`eval_app`].
//!   * `Builtin` — eval args directly, append upstream, call builtin.
//!   * `Return(Thunk)` / `Force(Variable)` — a thunk-shaped stage: the
//!     thunk is the callee, applied to `[upstream]` (or returned as-is when
//!     no upstream).  This closes the block-as-stage case
//!     (`5 | { |x| echo $x }`).
//!   * Anything else — evaluate as a plain comp (no upstream visible to it),
//!     then trampoline-apply upstream to the result.  The trampoline's
//!     existing arity errors do the right thing for `5 | 6`,
//!     `5 | { echo hi }`, etc.
//!
//! ## Step iteration
//!
//! When the upstream value structurally matches the Step protocol
//! (a variant `.more {head, tail: Thunk(_)}` or `.done`), `invoke`
//! drives demand-driven iteration: the consumer runs once per `.more`
//! head, the tail thunk is forced, and the loop terminates on `.done`.
//! The typechecker propagates the *element* type at this boundary
//! (see `apply_piped_value`), so the consumer is checked against `τ`
//! rather than the whole `Step τ`.  Any non-Step variant falls through
//! to the standard "upstream as last arg" path.

use crate::ir::{Comp, CompKind, Val};
use crate::step::{DONE_LABEL, HEAD_FIELD, MORE_LABEL, TAIL_FIELD};
use crate::types::*;

use super::{dispatch, eval_comp, eval_val, trampoline};

/// Push-with-clone for the upstream value — keeps the call sites tidy.
fn push_upstream(args: &mut Vec<Value>, upstream: Option<Value>) {
    if let Some(v) = upstream {
        args.push(v);
    }
}

/// If `upstream` is a Step-shaped variant, drive iteration by calling
/// `comp` once per element and forcing tail thunks until `.done`.
/// Returns `Some(result)` when iteration drove the call, `None` when
/// the upstream is not a Step (so the caller proceeds with the normal
/// "append as arg" path).
fn try_drive_step(
    comp: &Comp,
    upstream: &Option<Value>,
    shell: &mut Shell,
) -> Option<Result<Value, EvalSignal>> {
    let Some(v) = upstream.as_ref() else {
        return None;
    };
    let label = match v {
        Value::Variant { label, .. } => label,
        _ => return None,
    };
    if label != MORE_LABEL && label != DONE_LABEL {
        return None;
    }
    // For .more, ensure the payload has the Step shape: a record
    // carrying `head` and a `tail` thunk.  This keeps users free to
    // re-use the names `.more`/`.done` for unrelated variants — only
    // the structurally Step-shaped one triggers iteration.
    if label == MORE_LABEL {
        let Value::Variant { payload: Some(p), .. } = v else {
            return None;
        };
        let Value::Map(entries) = p.as_ref() else {
            return None;
        };
        let has_head = entries.iter().any(|(k, _)| k == HEAD_FIELD);
        let has_tail_thunk = entries
            .iter()
            .any(|(k, val)| k == TAIL_FIELD && matches!(val, Value::Thunk { .. }));
        if !(has_head && has_tail_thunk) {
            return None;
        }
    }
    Some(drive_step(comp, upstream.clone().unwrap(), shell))
}

/// Iterate a Step-shaped value, invoking `comp` per element.
/// Errors short-circuit — partial iteration leaves any unforced tail
/// untouched, which is the demand-driven point.
fn drive_step(comp: &Comp, mut step: Value, shell: &mut Shell) -> Result<Value, EvalSignal> {
    loop {
        match step {
            Value::Variant { ref label, .. } if label == DONE_LABEL => {
                return Ok(Value::Unit);
            }
            Value::Variant {
                ref label,
                payload: Some(payload),
            } if label == MORE_LABEL => {
                let entries = match *payload {
                    Value::Map(entries) => entries,
                    other => {
                        return Err(shell.err(
                            format!(
                                "step: .more payload must be a record, got {}",
                                other.type_name()
                            ),
                            1,
                        ));
                    }
                };
                let mut head: Option<Value> = None;
                let mut tail: Option<Value> = None;
                for (k, v) in entries {
                    if k == HEAD_FIELD {
                        head = Some(v);
                    } else if k == TAIL_FIELD {
                        tail = Some(v);
                    }
                }
                let head =
                    head.ok_or_else(|| shell.err(format!("step: .more missing '{HEAD_FIELD}'"), 1))?;
                let tail =
                    tail.ok_or_else(|| shell.err(format!("step: .more missing '{TAIL_FIELD}'"), 1))?;

                invoke(comp, Some(head), shell)?;

                // Force the tail to obtain the next step.
                let next = match tail {
                    Value::Thunk { body, captured } => {
                        shell.with_child(&captured, |child| super::eval_block_body(&body, child))?
                    }
                    other => {
                        return Err(shell.err(
                            format!("step: .more 'tail' must be a Block, got {}", other.type_name()),
                            1,
                        ));
                    }
                };
                step = next;
            }
            other => {
                return Err(shell.err(
                    format!(
                        "step: pipeline upstream must be `.more`/`.done`, got {} {}",
                        other.type_name(),
                        other
                    ),
                    1,
                ));
            }
        }
    }
}

/// Invoke a call-shaped `comp` with an optional `upstream` value.
///
/// `upstream = None` reproduces the evaluator's normal App/Exec/Builtin
/// behaviour; `upstream = Some(v)` is what a pipeline stage receives —
/// `v` is appended to the call's arg list before invocation, preserving
/// the data-last reading promised by SPEC.md §4.2.  When `comp` is not a
/// recognisable call shape, the fallback is "evaluate the comp; apply
/// its result to the upstream if any."
pub(crate) fn invoke(
    comp: &Comp,
    upstream: Option<Value>,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    if let Some(result) = try_drive_step(comp, &upstream, shell) {
        return result;
    }
    match &comp.kind {
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

        CompKind::Builtin { name, args } => {
            let mut arg_vals = dispatch::eval_subcall(shell, |shell| {
                args.iter().map(|v| eval_val(v, shell)).collect::<Result<Vec<_>, _>>()
            })?;
            push_upstream(&mut arg_vals, upstream);
            crate::builtins::call(name, &arg_vals, shell)?
                .ok_or_else(|| shell.err(format!("internal error: builtin '{name}' missing"), 1))
        }

        // Thunk-shaped: the user put a callable in stage position.
        // Evaluate to a value, then either return it (no upstream) or
        // apply it to upstream via the trampoline.
        CompKind::Return(v @ Val::Thunk(_)) | CompKind::Force(v @ Val::Variable(_)) => {
            let callee = eval_val(v, shell)?;
            apply_to_upstream(callee, upstream, shell)
        }

        // Opaque comp.  Evaluate it (it never sees the upstream), then
        // apply the result to upstream if present.
        _ => {
            let result = eval_comp(comp, shell)?;
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
