//! Bundled coreutils / diffutils / ripgrep dispatch.
//!
//! These tools live inside the ral binary, not on PATH, so they need
//! a dispatch shape distinct from spawning a system process: call
//! `uumain` in-process when sinks and logical state agree with the
//! kernel-level fds, otherwise hop through the pipeline-stage helper
//! so `apply_env` can propagate overrides via a child subprocess
//! instead of mutating this process's env or cwd from a thread.
//!
//! Builds without the bundled feature set get a stub
//! [`run_uutils_in_process`] that errors with 127, so call sites in
//! [`super`] remain feature-clean.

use crate::syntax::ast::RedirectMode;
use crate::types::*;

use super::stdio::EvalRedirect;

#[cfg(all(
    any(unix, windows),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
use {
    super::redirect::{apply_redirects, bind_stdin_for_uutils, commit_atomics, restore_redirects},
    crate::ir::{CommandName, CommandWord, CompKind, RedirectV, Val, ValRedirectTarget},
    std::io::Write,
};

#[cfg(all(
    any(unix, windows),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
/// Fast-path predicate: is in-process `uumain` safe right now?
///
/// `uumain` writes to libc fd 1/2 and reads `std::env`/`current_dir`
/// directly, so it is sound only when ral's sinks are direct-to-fd
/// and ral's logical state agrees with the process state.  Any capture
/// buffer or audit Tee, or any `env_overrides` / `within [dir: …]` /
/// `cd`-mutated logical cwd that the kernel doesn't see, forces the
/// pipeline-helper detour: mutating process env or cwd from this
/// thread would race other ral threads (`spawn`, `par`, pipeline).
pub(crate) fn can_run_uutils_in_process(shell: &Shell) -> bool {
    use crate::io::Sink;
    let cwd_matches_process = match &shell.mobile.context.cwd.current {
        None => true,
        Some(p) => crate::path::process_cwd().is_some_and(|q| q == *p),
    };
    matches!(shell.turn.io.stdout, Sink::Terminal | Sink::External(_))
        && matches!(shell.turn.io.stderr, Sink::Stderr)
        && shell.mobile.context.env_overrides().is_empty()
        && shell.mobile.context.dir.is_none()
        && cwd_matches_process
}

/// Call `uumain` for `tool` in this process, with `redirects` applied
/// via the same fd-level plumbing the builtins use.  Stdio is restored
/// before return; a panicking tool surfaces as a 1-status error.
///
/// `can_run_uutils_in_process` must have admitted the call: the
/// caller's sink discipline and env/cwd state are read implicitly
/// through libc, not through `shell`.
///
/// A `<file` stdin redirect must already be installed by the caller —
/// its parked `Source` is bound onto fd 0 here, never re-opened — so the
/// read is checked and audited exactly once.
#[cfg(all(
    any(unix, windows),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
pub(crate) fn run_uutils_in_process(
    tool: &str,
    arg_strs: &[String],
    redirects: &[(u32, RedirectMode, EvalRedirect)],
    shell: &mut Shell,
) -> Settled<Value> {
    use crate::builtins::uutils;

    // Rust's userspace stdout/stderr buffers sit above the kernel
    // fd / Win32 std slot.  Flushing here keeps pending bytes aimed
    // at the parent destination instead of being re-targeted by the
    // redirect we're about to install.
    std::io::stdout().flush().ok();
    std::io::stderr().flush().ok();

    let guard = apply_redirects(redirects, shell)?;
    let _stdin_backup = bind_stdin_for_uutils(shell);

    uutils::reset_exit_code();

    // `can_run_uutils_in_process` already gates on
    // `cwd.current == process_cwd`, so a well-behaved tool sees an
    // unchanged cwd.  Snapshot it anyway: a misbehaving tool that
    // internally `chdir`s would otherwise leak its change into the
    // next builtin's process-cwd read.
    #[allow(clippy::disallowed_methods)]
    let saved_cwd = std::env::current_dir().ok();

    let os_args: Vec<std::ffi::OsString> = std::iter::once(std::ffi::OsString::from(tool))
        .chain(arg_strs.iter().map(std::ffi::OsString::from))
        .collect();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        uutils::uutils_invoke(tool, os_args)
    }));

    if let Some(cwd) = saved_cwd {
        #[allow(clippy::disallowed_methods)]
        let _ = std::env::set_current_dir(cwd);
    }

    // Flush uumain's buffered output through the redirect target
    // before unwinding the fd swap.
    std::io::stdout().flush().ok();
    std::io::stderr().flush().ok();

    drop(_stdin_backup);
    let commits = restore_redirects(guard);

    let exit_code = match result {
        Ok(code) => {
            let global = uutils::get_exit_code();
            if global == 0 { code } else { global }
        }
        Err(_) => {
            return Err(Break::Error(
                Error::new(format!("bundled tool '{tool}' panicked"), 1)
                    .at_loc(shell.turn.loc.source_loc(0)),
            ));
        }
    };

    if exit_code == 0 {
        commit_atomics(commits)?;
    }

    shell.mobile.control.last_status = exit_code;
    if exit_code == 0 {
        Ok(Value::Unit)
    } else {
        Err(Break::Error(
            Error::new(
                format!("'{tool}' exited with status {exit_code}"),
                exit_code,
            )
            .at_loc(shell.turn.loc.source_loc(tool.len())),
        ))
    }
}

/// Capture-active fallback: route a bundled invocation through a
/// synthetic single-stage pipeline so dispatch lands in
/// `--ral-pipeline-stage-helper`.
///
/// Used when [`can_run_uutils_in_process`] refuses the fast path
/// (capture Tee, `!{…}`, audit) and spawning the bare tool as a
/// system process would either ENOENT (Windows ships no `ls.exe`)
/// or contradict bundled-first policy.  Inside the helper the sinks
/// are fresh Terminal/Stderr, so the fast path fires there and the
/// helper's bytes flow back through the standard pump plumbing.
///
/// Args are emitted as pre-stringified literals; redirects round-trip
/// through `ValRedirectTarget` so the helper's own `eval_call_parts`
/// reproduces the same `EvalRedirect` shape.  The synthetic head uses
/// `CommandWord::External` so the helper does not re-resolve to a
/// same-named builtin or alias in the rehydrated child shell.
#[cfg(all(
    any(unix, windows),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
pub(crate) fn dispatch_uutils_via_pipeline(
    cmd: &CommandName,
    arg_strs: &[String],
    redirects: &[(u32, RedirectMode, EvalRedirect)],
    shell: &mut Shell,
) -> Raw<Value> {
    let val_args: crate::ir::Args = arg_strs
        .iter()
        .map(|s| {
            crate::source::Spanned::synthetic(crate::ir::ValListElem::Single(Val::String(
                s.clone(),
            )))
        })
        .collect();
    let val_redirects: Vec<RedirectV> = redirects
        .iter()
        .map(|(fd, mode, target)| {
            let val_target = match target {
                EvalRedirect::File(path) => ValRedirectTarget::File(Val::String(path.clone())),
                EvalRedirect::Fd(n) => ValRedirectTarget::Fd(*n),
            };
            RedirectV {
                fd: *fd,
                mode: *mode,
                target: val_target,
            }
        })
        .collect();
    let stage = std::sync::Arc::new(crate::source::Spanned::synthetic(CompKind::Exec(
        crate::ir::Exec {
            head: CommandWord::External(cmd.clone()),
            args: val_args,
            redirects: val_redirects,
        },
    )));
    // The synthetic stage is an external head: byte output, and an input
    // edge the checker would leave a fresh variable.  With no neighbour to
    // pin it, that variable grounds to the value channel — the wire the
    // annotation pass would write for this lone external stage.
    let wires = [crate::mode::Wire::EXTERNAL];
    // A lone external stage is collected over the wire and emits no
    // tail call, so its tail position has no effect ([`Tail::No`]).
    crate::runtime::pipeline::run_pipeline(std::slice::from_ref(&stage), &wires, Tail::No, shell)
}

/// No-bundled-features fallback so call sites stay feature-clean.
/// Errors with 127 — the same exit code the OS would produce for a
/// missing executable, which is the moral equivalent when the bundled
/// implementation isn't compiled in.
#[cfg(not(all(
    any(unix, windows),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
)))]
pub(crate) fn run_uutils_in_process(
    tool: &str,
    _arg_strs: &[String],
    _redirects: &[(u32, RedirectMode, EvalRedirect)],
    _shell: &mut Shell,
) -> Settled<Value> {
    Err(Break::Error(Error::new(
        format!("bundled tool '{tool}' is not available in this build"),
        127,
    )))
}
