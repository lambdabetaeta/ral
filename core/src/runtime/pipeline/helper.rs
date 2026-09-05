//! Child side of ral's hidden multicall flags, entered before the CLI.
//!
//! `--ral-pipeline-anchor` holds the pipeline pgid open so a fast-exiting
//! first stage cannot strand its successors — the only stage that still
//! re-execs; a ral-written stage runs on a thread of the parent process.
//! `--ral-bundled-tool` exchanges no frame at all: the inherited env, cwd,
//! stdio, process group and sandbox are the whole execution context.

pub(crate) const ANCHOR_FLAG: &str = "--ral-pipeline-anchor";

pub(crate) const BUNDLED_TOOL_FLAG: &str = "--ral-bundled-tool";

/// Hold the pipeline pgid open: block reading stdin to EOF — the parent's
/// `AnchorProcess::finish` closing its release pipe.  Every termination
/// signal is swallowed and reported instead (see `group.rs`); the three stop
/// signals are ignored outright, so the anchor never stops and never needs
/// resuming.
#[cfg(unix)]
fn serve_anchor() -> u8 {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        unsafe {
            libc::signal(sig, report_signal as *const () as libc::sighandler_t);
        }
    }
    for sig in [libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
        unsafe {
            libc::signal(sig, libc::SIG_IGN);
        }
    }
    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
    0
}

/// Async-signal-safe: one `write(2)` of the signal number to stdout.
#[cfg(unix)]
extern "C" fn report_signal(sig: libc::c_int) {
    let byte = [u8::try_from(sig).unwrap_or(0)];
    unsafe {
        libc::write(libc::STDOUT_FILENO, byte.as_ptr().cast(), 1);
    }
}

#[cfg(windows)]
fn serve_anchor() -> u8 {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe {
        SetConsoleCtrlHandler(None, 1);
    }
    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
    0
}

/// Build a helper command that re-execs the current ral binary.
#[cfg(unix)]
pub(crate) fn self_reexec(flag: &str) -> std::io::Result<crate::process::Launch> {
    let mut cmd = crate::sandbox::self_command()?;
    cmd.arg(flag);
    Ok(crate::process::Launch::from_command(cmd))
}

/// Build a helper command that re-execs the current ral binary.
///
/// Windows has no sandbox-pinned self-path, so this takes the live
/// `current_exe` and would follow an on-disk swap between launch and exec.
#[cfg(windows)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:self-reexec-windows] Builds the ral-re-exec Command for Windows pipeline-anchor / bundled-tool multicall subprocesses. Infrastructure spawn, not a model exec image — the model's exec surfaces at command::run, not here."
)]
pub(crate) fn self_reexec(flag: &str) -> std::io::Result<crate::process::Launch> {
    let exe = std::env::current_exe()?;
    let mut cmd = crate::process::Launch::new(exe);
    cmd.arg(flag);
    Ok(cmd)
}

/// Hidden anchor dispatch from the binary entrypoint; `None` when argv names
/// no anchor and the ordinary CLI should run.
pub fn try_run_pipeline_anchor() -> Option<u8> {
    let mut args = std::env::args_os();
    let _argv0 = args.next();
    let mode = args.next()?;
    #[cfg(unix)]
    {
        crate::sandbox::register_self_for_helpers();
        // Before the mode check, so every argv-bearing ral gets it: Rust's
        // runtime ignores SIGPIPE, but a ral child producing under a foreign
        // shell's pipeline must still die of it.  ral's own interior edges
        // never deliver it — the parent holds each read end — and parent-side
        // protocol writes mask it per write in `subprocess_codec`.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    (mode == ANCHOR_FLAG).then(serve_anchor)
}

/// Hidden bundled-tool dispatch from the binary entrypoint
/// (`ral --ral-bundled-tool <tool> <args...>`).
///
/// `args` is the post-`early_init` argv sans the binary name, so the OS
/// sandbox is already entered and the tool runs confined.  The exit code
/// comes from `invoke_bundled`.
#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep"))]
pub fn try_run_bundled_tool(args: &[String]) -> Option<u8> {
    use crate::uutils;

    let (flag, rest) = args.split_first()?;
    if flag != BUNDLED_TOOL_FLAG {
        return None;
    }
    let Some((tool, tool_args)) = rest.split_first() else {
        crate::diagnostic::cmd_error("ral", &format!("{BUNDLED_TOOL_FLAG} requires a tool name"));
        return Some(2);
    };
    if !uutils::is_uutils_tool(tool) {
        crate::diagnostic::cmd_error("ral", &format!("'{tool}' is not a bundled tool"));
        return Some(127);
    }

    let exit_code = uutils::invoke_bundled(tool, tool_args);
    #[allow(
        clippy::cast_sign_loss,
        reason = "clamp(0, 255) bounds the value to the u8 range before the cast"
    )]
    Some(exit_code.clamp(0, 255) as u8)
}

/// With no bundled tool linked in the sentinel is unreachable, but still
/// recognised, so it fails as a clear diagnostic rather than a clap usage
/// error.
#[cfg(not(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")))]
pub fn try_run_bundled_tool(args: &[String]) -> Option<u8> {
    let flag = args.first()?;
    if flag != BUNDLED_TOOL_FLAG {
        return None;
    }
    crate::diagnostic::cmd_error("ral", "no bundled tools are available in this build");
    Some(127)
}
