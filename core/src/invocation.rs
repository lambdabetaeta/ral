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

/// [`serve`] the role this process's own argv names.
///
/// The one expression every `main` and test constructor shares: a test
/// binary's re-exec of `current_exe()` is thus served before libtest sees
/// flags it would reject.
pub fn serve_process(installers: &'static [crate::engine::EngineInstaller]) -> Option<u8> {
    let argv: Vec<OsString> = std::env::args_os().skip(1).collect();
    serve(&classify(&argv), installers)
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
