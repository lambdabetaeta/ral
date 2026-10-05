//! Child side of ral's hidden multicall flags, entered before the CLI.
//! `--ral-pipeline-anchor` holds the pipeline pgid open so a fast-exiting first
//! stage cannot strand its successors.  `--ral-bundled-tool` exchanges no frame
//! at all: the inherited env, cwd, stdio, process group and sandbox are the
//! whole execution context.

pub(crate) const ANCHOR_FLAG: &str = "--ral-pipeline-anchor";

pub(crate) const BUNDLED_TOOL_FLAG: &str = "--ral-bundled-tool";

/// Block reading stdin to EOF — the parent's `AnchorProcess::finish` closing
/// its release pipe.  Every termination signal is swallowed and reported
/// instead; the three stop signals are ignored outright, so the anchor never
/// stops and never needs resuming.
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

/// Every console event is swallowed, Ctrl-Break included: the group's grace
/// is for its stages, never the anchor holding the group open.
#[cfg(windows)]
fn serve_anchor() -> u8 {
    extern "system" fn swallow(_: u32) -> windows_sys::core::BOOL {
        windows_sys::Win32::Foundation::TRUE
    }
    // SAFETY: `swallow` has `PHANDLER_ROUTINE`'s signature; `TRUE` adds it.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(swallow), 1);
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
    reason = "[silent:self-reexec-windows] Builds the ral-re-exec Command for Windows pipeline-anchor / bundled-tool multicall subprocesses. Infrastructure spawn, not a model exec image — the model's exec surfaces at command::run, not here."
)]
pub(crate) fn self_reexec(flag: &str) -> std::io::Result<crate::process::Launch> {
    let exe = std::env::current_exe()?;
    let mut cmd = crate::process::Launch::new(exe);
    cmd.arg(flag);
    Ok(cmd)
}

/// Serve the pipeline anchor, which exits before [`crate::sandbox::early_init`]
/// would pin it, with SIGPIPE at its default.
pub fn serve_pipeline_anchor() -> u8 {
    #[cfg(unix)]
    {
        crate::sandbox::register_self_for_helpers();
        crate::uutils::init_signal_dispositions();
    }
    serve_anchor()
}

/// Serve the bundled-tool multicall (`ral --ral-bundled-tool <tool> <args...>`):
/// unconfined, run under whatever sandbox the process already inherited.
/// `args` is the tool's name, then its arguments.
pub(crate) fn serve_bundled_tool(args: &[std::ffi::OsString]) -> u8 {
    let Some((tool, tool_args)) = args.split_first() else {
        crate::diagnostic::cmd_error("ral", &format!("{BUNDLED_TOOL_FLAG} requires a tool name"));
        return 2;
    };
    run_bundled(&tool.to_string_lossy(), tool_args.to_vec())
}

/// Run bundled `tool` in this process: the multicall's body, and a confined
/// warrant's once its confinement is entered.
#[cfg(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep"))]
pub(crate) fn run_bundled(tool: &str, args: Vec<std::ffi::OsString>) -> u8 {
    use crate::uutils;
    // Rust's runtime ignores SIGPIPE, which would turn a write to a closed
    // pipe into an error exit.
    #[cfg(unix)]
    uutils::init_signal_dispositions();
    if !uutils::is_uutils_tool(tool) {
        crate::diagnostic::cmd_error("ral", &format!("'{tool}' is not a bundled tool"));
        return 127;
    }
    u8::try_from(uutils::invoke_bundled(tool, args).clamp(0, 255)).unwrap_or(u8::MAX)
}

/// With no bundled tool linked in, a tool is unreachable, but still answered
/// with a clear diagnostic rather than a clap usage error.
#[cfg(not(any(feature = "coreutils", feature = "diffutils", feature = "ripgrep")))]
pub(crate) fn run_bundled(_tool: &str, _args: Vec<std::ffi::OsString>) -> u8 {
    crate::diagnostic::cmd_error("ral", "no bundled tools are available in this build");
    127
}
