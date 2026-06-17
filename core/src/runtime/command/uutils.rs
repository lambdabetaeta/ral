//! Bundled coreutils / diffutils / ripgrep dispatch.
//!
//! These tools live inside the ral binary, not on PATH, so they need
//! a dispatch shape distinct from spawning a system process: call
//! `uumain` in-process via [`run_uutils_in_process`] when sinks and
//! logical state agree with the kernel-level fds (the
//! [`can_run_uutils_in_process`] fast path), otherwise spawn the
//! `ral --ral-bundled-tool <tool>` command image as an ordinary child
//! so `apply_env` can propagate overrides via that subprocess instead
//! of mutating this process's env or cwd from a thread.  Both
//! `run_uutils_in_process` and the bundled feature set it needs are
//! gated on the same cfg, so the inline fast path is the only call site.

#[cfg(all(
    any(unix, windows),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
use {crate::types::*, super::redirect::bind_stdin_for_uutils, std::io::Write};

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
/// child placement: mutating process env or cwd from this thread would
/// race other ral threads (`spawn`, `par`, pipeline).  An active
/// sandbox projection also refuses the inline path: an in-process tool
/// would run unconfined and escape the projection the child placement
/// pins via `sandbox::self_command`.  The projection fold returns
/// `None` immediately when no grant is active, so the extra check stays
/// cheap on the common path.
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
        && shell.sandbox_projection().is_none()
}

/// Call `uumain` for `tool` in this process.  Stdio is restored before
/// return; a panicking tool surfaces as a 1-status error.
///
/// `can_run_uutils_in_process` must have admitted the call: the
/// caller's sink discipline and env/cwd state are read implicitly
/// through libc, not through `shell`.  Admission implies no call-site
/// redirects (`Sink::Terminal | External` with `redirects.is_empty()`),
/// so this path wires no `> file` / `2>&1` plumbing.
///
/// Stdin is still bound: the inline gate constrains the *output* sinks,
/// not the input, so a function whose body is a bundled tool can carry a
/// parent `Pipe` / `File` on `shell.turn.io.stdin`.
/// [`bind_stdin_for_uutils`] aliases that source onto fd 0 for the
/// in-process tool and restores fd 0 on drop.
#[cfg(all(
    any(unix, windows),
    any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")
))]
pub(crate) fn run_uutils_in_process(
    tool: &str,
    arg_strs: &[String],
    shell: &mut Shell,
) -> Settled<Value> {
    use crate::builtins::uutils;

    // Rust's userspace stdout/stderr buffers sit above the kernel
    // fd / Win32 std slot.  Flushing here keeps pending bytes aimed
    // at the parent destination instead of being re-targeted by the
    // stdin alias we're about to install.
    std::io::stdout().flush().ok();
    std::io::stderr().flush().ok();

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

    // Flush uumain's buffered output before unwinding the stdin alias.
    std::io::stdout().flush().ok();
    std::io::stderr().flush().ok();

    drop(_stdin_backup);

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
