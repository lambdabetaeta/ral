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
        Value::Thunk { .. } => {
            // Redirects on a closure call apply to the body's fd context: open
            // the targets, dup over fd 0/1/2, then evaluate.  Flush stdout/err
            // before restoring fds so buffered bytes go to the redirect target,
            // not back to the terminal.  Always restore; commit atomic writes
            // only if the body succeeded.
            if redirects.is_empty() {
                trampoline(name.clone(), args.to_vec(), shell)
            } else {
                let guard = exec::apply_redirects(redirects, shell)?;
                let result = trampoline(name.clone(), args.to_vec(), shell);
                use std::io::Write;
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().flush();
                let _ = shell.io.stdout.flush();
                let commits = exec::restore_redirects(guard);
                match result {
                    Ok(v) => {
                        exec::commit_atomics(commits)?;
                        Ok(v)
                    }
                    Err(e) => Err(e),
                }
            }
        }
        // No args, no redirects → identity (e.g. bare variable reference).
        _ if args.is_empty() && redirects.is_empty() => Ok(name.clone()),
        _ => Err(shell.err_hint(
            format!("{} is not a function", name.type_name()),
            "only Lambdas and Blocks are functions; use command syntax for executables",
            1,
        )),
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

    let bare = name.bare();

    // Step 1: effect handlers.  Per-name handlers fire unconditionally.
    // Catch-all handlers skip builtins/aliases (they are language-internal);
    // ^name (external_only) always fires the catch-all — nothing escapes.
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
            return invoke_handler(thunk, is_catch_all, depth, name, args, shell);
        }
    }

    // Step 2: ^name — resolve via PATH only, skip alias/builtin/prelude.
    if external_only {
        return run_external(name, args, redirects, shell);
    }

    // Step 3: normal command-head chain.
    let head = bare
        .map(|name| shell.classify_command_head(name))
        .unwrap_or(CommandHead::External);
    #[cfg(debug_assertions)]
    if let Some(name) = bare {
        crate::dbg_trace!(
            "dispatch",
            "name={name} head={head:?} aliases={:?}",
            shell.registry.aliases.keys().collect::<Vec<_>>()
        );
    }
    match (head, bare) {
        (CommandHead::Alias, Some(name)) => {
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
        (CommandHead::Builtin, Some(name)) => {
            let start_us = audit::start(shell);
            let redir_state = exec::apply_redirects(redirects, shell)?;
            let result = crate::builtins::call(name, args, shell);
            // Always restore fds; commit atomic writes only if the builtin
            // succeeded, otherwise drop the commits to remove tmp files.
            let commits = exec::restore_redirects(redir_state);
            match result? {
                Some(v) => {
                    exec::commit_atomics(commits)?;
                    audit::record_exec(shell, name, args, &v, start_us);
                    Ok(v)
                }
                None => Err(shell.err(format!("internal error: builtin not found: {name}"), 1)),
            }
        }
        (CommandHead::GrantDenied, Some(name)) => {
            audit::record_deny(shell, name, args);
            Err(shell.err_hint(
                format!("command '{name}' denied by active grant"),
                "add the command to the grant exec map to allow it",
                1,
            ))
        }
        _ => run_external(name, args, redirects, shell),
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
    let is_lambda = matches!(&thunk, Value::Thunk { body, .. } if matches!(body.as_ref().kind, CompKind::Lam { .. }));
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
