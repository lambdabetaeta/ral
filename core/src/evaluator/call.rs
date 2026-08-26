//! Application dispatch: `App` is CBPV application, `Exec` an external
//! command. Pipelines use the same dispatch, with byte transport handled by
//! the runtime pipeline.

use crate::ir::{Args, Comp, CompKind, RedirectV, ValListElem};
use crate::types::{Error, Mooring, Raw, Shell, Tail, TailCall, Value};
use std::sync::Arc;

use super::comp::eval_comp;
use super::val::{close, spread_type_err};
use super::{apply, redirect};
use crate::runtime::command::EvalRedirectV;
use crate::runtime::command_call;

/// Dispatches `App`, `Exec`, and redirect-scoped calls; anything else falls
/// through to [`eval_comp`].
pub(crate) fn invoke(
    comp: &Arc<Comp>,
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    match &comp.item {
        CompKind::Exec(e) => {
            let (arg_vals, redir_eval) = eval_call_parts(&e.args, &e.redirects, shell)?;
            command_call::run_call(&e.head, &arg_vals, &redir_eval, comp.span, mooring, shell)
        }

        CompKind::App {
            head,
            args: app_args,
            ..
        } => {
            // Evaluated, not tail-called: the application consumes their values.
            let head_val = eval_comp(head, mooring, shell, Tail::No)?;
            let arg_vals = eval_call_args(app_args, shell)?;
            eval_app(head_val, arg_vals, tail, mooring, shell)
        }

        CompKind::Redirect { body, redirects } => {
            // Forwarding `tail` is safe: the frame absorbs the tail call
            // inside itself, so the callee still sees the redirected fds.
            redirect::within_redirect_frame(redirects, mooring, shell, |shell| {
                invoke(body, tail, mooring, shell)
            })
        }

        _ => eval_comp(comp, mooring, shell, Tail::No),
    }
}

/// Eliminator for CBPV application: a Lambda or Block in tail position yields
/// a [`TailCall`] for the trampoline instead. `args` must be non-empty.
fn eval_app(
    name: Value,
    args: Vec<Value>,
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    debug_assert!(
        !args.is_empty(),
        "App with empty args reached the evaluator"
    );
    match name {
        Value::Thunk(_) | Value::Native { .. } if tail == Tail::Yes => {
            Err(TailCall { callee: name, args }.into())
        }
        _ => apply(name, args, mooring, shell).map_err(Into::into),
    }
}

/// Evaluates an `Exec`'s arguments and trailing redirects together. Also the
/// argv path for a directly-spawned external stage, in
/// `runtime::pipeline::resolve`.
pub(crate) fn eval_call_parts(
    args: &Args,
    redirects: &[RedirectV],
    shell: &mut Shell,
) -> Result<(Vec<Value>, Vec<EvalRedirectV>), Error> {
    let redir_eval = redirect::eval_redirects(redirects, shell)?;
    let arg_vals = eval_call_args(args, shell)?;
    Ok((arg_vals, redir_eval))
}

fn eval_call_args(args: &Args, shell: &mut Shell) -> Result<Vec<Value>, Error> {
    let mut out = Vec::with_capacity(args.len());
    for elem in args {
        match &elem.item {
            ValListElem::Single(v) => out.push(close(v, &shell.env)?),
            ValListElem::Spread(v) => {
                let spread = close(v, &shell.env)?;
                let Value::List(list) = spread else {
                    return Err(spread_type_err(&spread));
                };
                out.extend(list);
            }
        }
    }
    Ok(out)
}
