//! Implements application according to the kind of computation:
//! `App` is CBPV function application; `Exec` is external command
//! execution. The same code path is used in pipelines, so the entry
//! points carry an `upstream` argument which may carry a value
//! produced by the previous pipeline stage.
//!
//! Two crate-visible entry points:
//!
//! - [`invoke`] — dispatches `App`, `Exec`, and redirect-scoped
//!   calls. When `upstream` is present, it is appended to the call's
//!   argument list. Computations that are not call-shaped (a bare
//!   variable reference `$f`, a literal block, ...) fall through to
//!   `eval_comp`; any upstream value is then applied to the
//!   resulting value via [`apply`].
//!
//! - [`eval_call_parts`] — evaluates the arguments and trailing
//!   redirects of an `Exec` together. Shared with the pipeline
//!   External case in [`crate::runtime::pipeline::resolve`].
//!
//! Internals: [`eval_app`] is the eliminator for CBPV application —
//! a Lambda or Block handed [`Tail::Yes`] emits [`TailCall`] for the
//! trampoline, otherwise it applies directly, and any other value is a
//! type error. [`eval_call_args`] walks the `Args` list, splicing
//! `Spread` elements. The head and arguments are always evaluated under
//! a non-trivial continuation ([`Tail::No`]); only the call itself
//! carries the enclosing computation's tail-ness.

use crate::ir::{Args, Comp, CompKind, RedirectV, ScopeOp, ValListElem};
use crate::types::*;
use std::sync::Arc;

use super::comp::eval_comp;
use super::val::{eval_val, spread_type_err};
use super::{apply, redirect};
use crate::runtime::command::EvalRedirectV;
use crate::runtime::command_call;

pub(crate) fn invoke(
    comp: &Arc<Comp>,
    upstream: Option<Value>,
    tail: Tail,
    shell: &mut Shell,
) -> Raw<Value> {
    match &comp.item {
        CompKind::Exec(e) => {
            let (mut arg_vals, redir_eval) = eval_call_parts(&e.args, &e.redirects, shell)?;
            // Append the value from the previous pipeline stage, if any.
            arg_vals.extend(upstream);
            command_call::run_call(&e.head, &arg_vals, &redir_eval, shell)
        }

        CompKind::App {
            head,
            args: app_args,
            ..
        } => {
            // The head and arguments are evaluated, not tail-called:
            // they produce values the application consumes.
            let head_val = eval_comp(head, shell, Tail::No)?;
            let mut arg_vals = eval_call_args(app_args, shell)?;
            arg_vals.extend(upstream);
            // CBPV application requires at least one argument; a spread
            // of an empty list (`f ...$xs` with `$xs == []`) can leave
            // the argument list empty, in which case the App reduces to
            // its head.
            if arg_vals.is_empty() {
                Ok(head_val)
            } else {
                eval_app(head_val, arg_vals, tail, shell)
            }
        }

        CompKind::Scope(ScopeOp::Redirect { body, redirects }) => {
            redirect::within_redirect_frame(redirects, shell, |shell| {
                invoke(body, upstream, tail, shell)
            })
        }

        _ => {
            let result = eval_comp(comp, shell, Tail::No)?;
            match upstream {
                None => Ok(result),
                // Applying the upstream value to a bare-value stage (a
                // function reference consuming `x | f`) is itself the
                // stage's tail call: route through `eval_app` so a
                // granted tail position emits a [`TailCall`] and a final
                // pipeline stage trampolines rather than recurses.
                Some(v) => eval_app(result, vec![v], tail, shell),
            }
        }
    }
}

/// Eliminator for CBPV application. A Lambda or Block handed
/// [`Tail::Yes`] emits [`TailCall`] for the trampoline; otherwise it
/// applies directly. Any other value is a type error. The caller
/// guarantees `args` is non-empty.
fn eval_app(name: Value, args: Vec<Value>, tail: Tail, shell: &mut Shell) -> Raw<Value> {
    debug_assert!(
        !args.is_empty(),
        "App with empty args reached the evaluator"
    );
    match name {
        Value::Lambda { .. } | Value::Block { .. } if tail == Tail::Yes => {
            Err(TailCall { callee: name, args }.into())
        }
        _ => apply(name, args, shell).map_err(Into::into),
    }
}

/// Evaluates the arguments and trailing redirects of an `Exec`
/// together. Shared between [`invoke`]'s `Exec` arm and the pipeline
/// External case in [`crate::runtime::pipeline::resolve`].
///
/// Upstream values from pipeline stages are appended later, in
/// `pipeline::invoke`, not here.
pub(crate) fn eval_call_parts(
    args: &Args,
    redirects: &[RedirectV],
    shell: &mut Shell,
) -> Result<(Vec<Value>, Vec<EvalRedirectV>), Error> {
    let redir_eval = redirect::eval_redirects(redirects, shell)?;
    let arg_vals = eval_call_args(args, shell)?;
    Ok((arg_vals, redir_eval))
}

/// Flattens an `Args` list into a positional `Vec<Value>`: `Single`
/// elements contribute one value, `Spread` elements splice an
/// evaluated list.
fn eval_call_args(args: &Args, shell: &mut Shell) -> Result<Vec<Value>, Error> {
    let mut out = Vec::with_capacity(args.len());
    for elem in args {
        match &elem.item {
            ValListElem::Single(v) => out.push(eval_val(v, shell)?),
            ValListElem::Spread(v) => {
                let spread = eval_val(v, shell)?;
                let Value::List(list) = spread else {
                    return Err(spread_type_err(shell, &spread));
                };
                out.extend(list);
            }
        }
    }
    Ok(out)
}
