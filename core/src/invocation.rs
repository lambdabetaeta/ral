//! Shared process-role dispatch for `ral`, `exarch` and test constructors.
//!
//! A warrant must enter confinement before opening or pinning anything.
//! The anchor spawns nothing, so needs no pin; a bundled tool pins but reaches
//! no session ledger. Engines and shells boot the sandbox support for launches.

use crate::site::{Silent, site};
use crate::{Invocation, Role, authority, classify, io::terminal, uutils};
use std::ffi::OsString;

/// Serve `role`, returning its exit code or leaving normal startup to the caller.
#[cfg_attr(
    not(unix),
    allow(unused_variables, reason = "only a Unix engine reads its installers")
)]
pub fn serve(
    role: &Invocation<'_>,
    installers: &'static [crate::engine::EngineInstaller],
) -> Option<u8> {
    match *role {
        Invocation::Warrant(args) => Some(authority::sandbox::serve_warrant(args)),
        Invocation::PipelineAnchor => Some(crate::shell::exec::pipeline::anchor::serve()),
        #[cfg(unix)]
        Invocation::Engine => {
            authority::sandbox::boot();
            crate::carrier::run_engine(installers)
        }
        Invocation::BundledTool(args) => {
            authority::sandbox::pin();
            Some(serve_bundled_tool(args))
        }
        Invocation::Shell => {
            authority::sandbox::boot();
            None
        }
    }
}

/// Exit after [`serve`] finishes a role; continue normal startup otherwise.
/// Role handlers must finish cleanup before returning an exit code.
pub fn exit_if_served(code: Option<u8>) {
    if let Some(code) = code {
        site!(Silent::ServedRoleExit, std::process::exit(i32::from(code)));
    }
}

/// Serve this process's role from `main`, using std's argv without requiring `/proc`.
pub fn serve_process(installers: &'static [crate::engine::EngineInstaller]) -> Option<u8> {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    serve(&classify(&argv), installers)
}

/// Serve a test re-exec before libtest can reject its role flag.
///
/// Without glibc on Linux, std's argv is still empty here. [`argv_before_main`]
/// supplies the role, but cannot initialise std's argv for uucore; bundled-tool
/// tests are therefore ignored under `cfg(ctor_without_argv)`.
pub fn serve_process_before_main(
    installers: &'static [crate::engine::EngineInstaller],
) -> Option<u8> {
    let args = argv_before_main();
    let invocation = classify(&args);
    #[cfg(any(test, feature = "test-util"))]
    if matches!(invocation, Invocation::Shell)
        && let Some(code) = crate::test_helper::confinement::probe::serve()
    {
        return Some(code);
    }
    serve(&invocation, installers)
}

/// Read argv without `argv[0]` before std captures it on non-glibc Linux.
/// Unlike glibc, musl passes no argv to `.init_array` constructors.
/// Failure is fatal: missing the role would leave libtest to reject its flag.
#[cfg(ctor_without_argv)]
fn argv_before_main() -> Vec<OsString> {
    use std::os::unix::ffi::OsStringExt;
    let cmdline = site!(Silent::ArgvBeforeMain, std::fs::read("/proc/self/cmdline"))
        .unwrap_or_else(|e| {
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

/// Read argv without `argv[0]` where std makes it available before `main`.
#[cfg(not(ctor_without_argv))]
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
    uutils::run(tool, args.to_vec()).unwrap_or_else(|e| {
        terminal::cmd_error("ral", &e.to_string());
        127
    })
}
