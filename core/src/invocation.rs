//! The one pre-`main` dispatch, for `ral`, `exarch` and every test constructor.
//!
//! Each process serves the role [`classify`](crate::classify) names, or is
//! left, pinned, to its caller; each role's body stays with the code it serves.
//!
//! The order is the sandbox's.  A confined re-exec is served by
//! [`sandbox::serve_warrant`] alone, pinning and opening nothing before it is
//! confined; the anchor and the pgid probe spawn nothing, so pin nothing.  A
//! bundled tool pins, as a re-exec child that reaches no session ledger.  Every
//! other role boots first, the engine included: its grant-confined launches
//! need the pinned envelope as much as the shell's.

#[cfg(all(unix, any(test, feature = "test-util")))]
mod probe;

use crate::{Invocation, Role, classify, sandbox, terminal, uutils};
use std::ffi::OsString;

/// Serve `role`.  `Some(code)` is a served role's exit; `None` leaves the
/// process to its caller: the shell, whose argv it reads.
#[cfg_attr(
    not(unix),
    allow(unused_variables, reason = "only a Unix engine reads its installers")
)]
pub fn serve(
    role: &Invocation<'_>,
    installers: &'static [crate::engine::EngineInstaller],
) -> Option<u8> {
    match *role {
        Invocation::Warrant(extra) => Some(sandbox::serve_warrant(extra)),
        Invocation::PipelineAnchor => Some(crate::runtime::pipeline::anchor::serve()),
        #[cfg(all(unix, any(test, feature = "test-util")))]
        Invocation::PgidCheck { tag } => Some(probe::pgid_check(tag)),
        #[cfg(unix)]
        Invocation::Engine => {
            sandbox::boot();
            crate::carrier::run_engine(installers)
        }
        Invocation::BundledTool(args) => {
            sandbox::pin();
            Some(serve_bundled_tool(args))
        }
        #[cfg(all(unix, any(test, feature = "test-util")))]
        Invocation::DetachBirth { trace, marker } => {
            sandbox::boot();
            Some(probe::detach_birth(trace, marker))
        }
        Invocation::Shell => {
            sandbox::boot();
            None
        }
    }
}

/// [`serve`] the role this process's own argv names: the `main`s' form.
///
/// A `main` reads argv from std, as any program does, and never from `/proc`:
/// the guest and a sandboxed payload may have none.  A `#[ctor]` runs before
/// `main` and calls [`serve_process_before_main`] instead, so that a test
/// binary's re-exec of `current_exe()` is served before libtest sees flags it
/// would reject.
pub fn serve_process(installers: &'static [crate::engine::EngineInstaller]) -> Option<u8> {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    serve(&classify(&argv), installers)
}

/// [`serve_process`], for a `#[ctor]`: the same role, read from the argv
/// [`argv_before_main`] can find before `main` has run.
///
/// It exists because a constructor cannot read argv the way a `main` does: on
/// Linux without glibc std has not captured it yet, and a constructor that saw
/// no argv would see no role, leaving libtest to reject the role's flag.
pub fn serve_process_before_main(
    installers: &'static [crate::engine::EngineInstaller],
) -> Option<u8> {
    serve(&classify(&argv_before_main()), installers)
}

/// This process's argv without `argv[0]`, as a `#[ctor]` can read it.
///
/// std captures argv before `main` only where the platform hands it over:
/// glibc passes `argc` and `argv` to `.init_array` functions, and std
/// registers one on `all(target_os = "linux", target_env = "gnu")`; macOS has
/// `_NSGetArgv` and Windows `GetCommandLineW`.  musl calls its `.init_array`
/// functions with no arguments, so there `std::env::args_os` is empty until
/// `main`.  The kernel's copy of the exec'd argv, `/proc/self/cmdline`, is the
/// one source left, so that is read, and a failure to read it is a panic: a
/// constructor that silently saw no role would fail much later, and far from
/// its cause.
#[cfg(all(target_os = "linux", not(target_env = "gnu")))]
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:argv-before-main] Bootstrap read of the kernel's copy of this process's own argv, where std has none before `main`. Process bootstrap, not model data I/O."
)]
fn argv_before_main() -> Vec<OsString> {
    use std::os::unix::ffi::OsStringExt;
    let cmdline = std::fs::read("/proc/self/cmdline").unwrap_or_else(|e| {
        panic!(
            "a pre-main constructor on a non-glibc Linux has no argv but /proc/self/cmdline, \
             and it could not be read: {e}"
        )
    });
    // Each argument ends in a NUL, so the last piece of a plain split is empty.
    let args = cmdline.strip_suffix(&[0]).unwrap_or(&cmdline);
    args.split(|&b| b == 0)
        .skip(1)
        .map(|arg| OsString::from_vec(arg.to_vec()))
        .collect()
}

/// This process's argv without `argv[0]`: std has captured it before `main`
/// here (see the Linux-without-glibc form for where it has not).
#[cfg(not(all(target_os = "linux", not(target_env = "gnu"))))]
fn argv_before_main() -> Vec<OsString> {
    std::env::args_os().skip(1).collect()
}

/// `ral --ral-bundled-tool <tool> <args...>`: unconfined, under whatever
/// sandbox the process already inherited.
fn serve_bundled_tool(args: &[OsString]) -> u8 {
    let Some((tool, args)) = args.split_first() else {
        terminal::cmd_error(
            "ral",
            &format!("{} requires a tool name", Role::BundledTool.flag()),
        );
        return 2;
    };
    uutils::run(&tool.to_string_lossy(), args.to_vec()).unwrap_or_else(|e| {
        terminal::cmd_error("ral", &e.to_string());
        127
    })
}
