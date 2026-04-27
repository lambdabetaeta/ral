//! Command dispatch, call-argument evaluation, and effect-handler invocation.

use super::exec::{self, EvalRedirect};
use super::{audit, eval_val, trampoline};
use crate::ast::RedirectMode;
use crate::ir::*;
use crate::types::*;

// ── Call-argument evaluation ─────────────────────────────────────────────

/// Clear `in_tail_position` while evaluating call subexpressions, restore after.
pub(crate) fn eval_subcall<T, F>(shell: &mut Shell, f: F) -> Result<T, EvalSignal>
where
    F: FnOnce(&mut Shell) -> Result<T, EvalSignal>,
{
    super::with_tail(shell, false, f)
}

/// Evaluate pipeline-in value, redirect targets, and argument values.
#[allow(clippy::type_complexity)]
pub(crate) fn eval_call_parts(
    args: &[Val],
    redirects: &[(u32, RedirectMode, ValRedirectTarget)],
    shell: &mut Shell,
) -> Result<(Vec<Value>, Vec<(u32, RedirectMode, EvalRedirect)>), EvalSignal> {
    let piped = shell.io.value_in.take();
    let redir_eval = redirects
        .iter()
        .map(|(fd, mode, target)| {
            Ok((
                *fd,
                *mode,
                match target {
                    ValRedirectTarget::File(v) => EvalRedirect::File(eval_val(v, shell)?.to_string()),
                    ValRedirectTarget::Fd(n) => EvalRedirect::Fd(*n),
                },
            ))
        })
        .collect::<Result<Vec<_>, EvalSignal>>()?;
    let arg_vals = eval_call_args(args, piped, shell)?;
    Ok((arg_vals, redir_eval))
}

/// Evaluate argument list, expanding `...$xs` spreads.
/// Shared by the normal call path and `pipeline::analyze_stage`.
pub(crate) fn eval_call_args(
    args: &[Val],
    piped: Option<Value>,
    shell: &mut Shell,
) -> Result<Vec<Value>, EvalSignal> {
    let mut arg_vals = Vec::with_capacity(args.len() + usize::from(piped.is_some()));
    for arg in args {
        let v = eval_val(arg, shell)?;
        if let Val::List(elems) = arg
            && matches!(elems.as_slice(), [ValListElem::Spread(_)])
            && let Value::List(items) = v
        {
            arg_vals.extend(items);
            continue;
        }
        arg_vals.push(v);
    }
    if let Some(piped) = piped {
        arg_vals.push(piped);
    }
    Ok(arg_vals)
}

// ── Application dispatch ─────────────────────────────────────────────────

/// Run `body` with `redirects` applied: open the targets, dup over fd 0/1/2,
/// run, then always restore.  Atomic-write commits fire on success and are
/// dropped on failure (so the tmp file is removed).  When redirects are
/// non-empty, stdout/stderr are flushed before restoring fds so buffered
/// bytes land at the redirect target rather than back at the terminal.
fn with_redirects<F>(
    redirects: &[(u32, RedirectMode, EvalRedirect)],
    shell: &mut Shell,
    body: F,
) -> Result<Value, EvalSignal>
where
    F: FnOnce(&mut Shell) -> Result<Value, EvalSignal>,
{
    if redirects.is_empty() {
        return body(shell);
    }
    let guard = exec::apply_redirects(redirects, shell)?;
    let result = body(shell);
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    let _ = shell.io.stdout.flush();
    let commits = exec::restore_redirects(guard);
    let v = result?;
    exec::commit_atomics(commits)?;
    Ok(v)
}

pub(crate) fn eval_app(
    name: &Value,
    args: &[Value],
    redirects: &[(u32, RedirectMode, EvalRedirect)],
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    match name {
        // Tail-position thunk with args → emit TailCall for the trampoline.
        // Redirects are not part of the TailCall payload, so disable TCO
        // whenever redirects are present — they need an apply/restore frame.
        Value::Thunk { .. }
            if !args.is_empty()
                && shell.control.in_tail_position
                && redirects.is_empty() =>
        {
            Err(EvalSignal::TailCall {
                callee: name.clone(),
                args: args.to_vec(),
            })
        }
        Value::Thunk { .. } => with_redirects(redirects, shell, |shell| {
            trampoline(name.clone(), args.to_vec(), shell)
        }),
        // No args, no redirects → identity (e.g. bare variable reference).
        _ if args.is_empty() && redirects.is_empty() => Ok(name.clone()),
        _ => Err(shell.err_hint(
            format!("{} is not a function", name.type_name()),
            "only Lambdas and Blocks are functions; use command syntax for executables",
            1,
        )),
    }
}

/// Classification of an `Exec` call to one of five terminal dispatch arms.
///
/// `Handler` carries the looked-up thunk so the caller can invoke it
/// without redoing the lookup; the other arms are pure tags.  Both
/// `dispatch_by_name` and the pipeline analyzer go through this function so
/// the rules (handler priority, `^name` semantics, alias/builtin/grant
/// classification) live in exactly one place.
pub(crate) enum Dispatch {
    /// An effect handler intercepts.  `is_catch_all` and `depth` are the
    /// fields shallow-handler invocation needs.
    Handler {
        thunk: Value,
        is_catch_all: bool,
        depth: usize,
    },
    Alias,
    Builtin,
    GrantDenied,
    External,
}

/// Classify a command head.  Handlers fire first (per-name unconditionally;
/// catch-all unless dominated by a builtin or alias when `external_only`
/// is false).  `^name` then short-circuits to External.  Otherwise the
/// head classifier picks Alias / Builtin / GrantDenied / External.
///
/// Pure: takes `&Shell`, performs no I/O and no shell mutation.  Pipeline
/// analysis can call it from `analyze_stage` without disturbing the
/// surrounding evaluator state.
pub(crate) fn classify_dispatch(name: &ExecName, external_only: bool, shell: &Shell) -> Dispatch {
    let bare = name.bare();

    if let Some(name) = bare
        && let Some((thunk, is_catch_all, depth)) = shell.lookup_handler(name)
    {
        let dominated = is_catch_all
            && !external_only
            && matches!(
                shell.classify_command_head(name),
                CommandHead::Builtin | CommandHead::Alias
            );
        if !dominated {
            return Dispatch::Handler {
                thunk,
                is_catch_all,
                depth,
            };
        }
    }

    if external_only {
        return Dispatch::External;
    }

    match bare
        .map(|n| shell.classify_command_head(n))
        .unwrap_or(CommandHead::External)
    {
        CommandHead::Alias => Dispatch::Alias,
        CommandHead::Builtin => Dispatch::Builtin,
        CommandHead::GrantDenied => Dispatch::GrantDenied,
        CommandHead::External => Dispatch::External,
    }
}

pub(crate) fn dispatch_by_name(
    name: &ExecName,
    args: &[Value],
    redirects: &[(u32, RedirectMode, EvalRedirect)],
    external_only: bool,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    if !matches!(name.bare(), Some(name) if name.starts_with('_')) {
        shell.location.call_site.script = shell.location.script.clone();
        shell.location.call_site.line = shell.location.line;
        shell.location.call_site.col = shell.location.col;
    }

    let dispatch = classify_dispatch(name, external_only, shell);
    let bare = name.bare();

    #[cfg(debug_assertions)]
    if let Some(name) = bare {
        crate::dbg_trace!(
            "dispatch",
            "name={name} arm={} aliases={:?}",
            match &dispatch {
                Dispatch::Handler { .. } => "Handler",
                Dispatch::Alias => "Alias",
                Dispatch::Builtin => "Builtin",
                Dispatch::GrantDenied => "GrantDenied",
                Dispatch::External => "External",
            },
            shell.registry.aliases.keys().collect::<Vec<_>>()
        );
    }

    match dispatch {
        Dispatch::Handler {
            thunk,
            is_catch_all,
            depth,
        } => invoke_handler(thunk, is_catch_all, depth, bare.unwrap(), args, shell),
        Dispatch::Alias => {
            let name = bare.unwrap();
            let alias = shell.registry.aliases.get(name).cloned().unwrap();
            let alias_args = vec![Value::List(args.to_vec())];
            match &alias.origin {
                AliasOrigin::User => trampoline(alias.value, alias_args, shell),
                AliasOrigin::Plugin(pname) => shell
                    .with_registered_plugin_capabilities(pname, |shell| {
                        trampoline(alias.value, alias_args, shell)
                    }),
            }
        }
        Dispatch::Builtin => {
            let name = bare.unwrap();
            let start_us = audit::start(shell);
            let v = with_redirects(redirects, shell, |shell| {
                crate::builtins::call(name, args, shell)?.ok_or_else(|| {
                    shell.err(format!("internal error: builtin not found: {name}"), 1)
                })
            })?;
            audit::record_exec(shell, name, args, &v, start_us);
            Ok(v)
        }
        Dispatch::GrantDenied => {
            let name = bare.unwrap();
            audit::record_deny(shell, name, args);
            Err(shell.err_hint(
                format!("command '{name}' denied by active grant"),
                "add the command to the grant exec map to allow it",
                1,
            ))
        }
        Dispatch::External => run_external(name, args, redirects, shell),
    }
}

/// PATH-resolved external command with start/end audit framing.
fn run_external(
    name: &ExecName,
    args: &[Value],
    redirects: &[(u32, RedirectMode, EvalRedirect)],
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    let start_us = audit::start(shell);
    let result = exec::exec_external(name, args, redirects, shell);
    let shown = exec::render_exec_name(name, shell);
    audit::record_exec(
        shell,
        &shown,
        args,
        result.as_ref().ok().unwrap_or(&Value::Unit),
        start_us,
    );
    result
}

/// Invoke an effect handler (shallow-handler semantics: strip triggering frame
/// so the handler body doesn't see it, preventing infinite recursion).
fn invoke_handler(
    thunk: Value,
    is_catch_all: bool,
    depth: usize,
    name: &str,
    args: &[Value],
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    let len = shell.dynamic.handler_stack.len();
    let stripped = shell.dynamic.handler_stack.split_off(len - depth);
    let is_lambda = matches!(
        &thunk,
        Value::Thunk { body, .. } if matches!(body.as_ref().kind, CompKind::Lam { .. })
    );
    let call_args = if is_catch_all {
        vec![Value::String(name.into()), Value::List(args.to_vec())]
    } else if is_lambda {
        vec![Value::List(args.to_vec())]
    } else {
        vec![]
    };
    let result = trampoline(thunk, call_args, shell);
    shell.dynamic.handler_stack.extend(stripped);
    result
}
