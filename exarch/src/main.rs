//! Exarch binary entry point — a thin shell over [`exarch::run`].  All
//! the agent lives in the library crate so integration tests can link
//! it directly; `main` only wires the pre-`main` helper trampoline and
//! the sandbox dispatch that must run before the frontend.

fn main() -> std::process::ExitCode {
    #[cfg(unix)]
    ral_core::builtins::uutils::init_signal_dispositions();
    // Teach core how to dress a sandbox-IPC child's fresh shell with
    // exarch's host builtins, then serve any pipeline-stage / test
    // helper re-exec.  Must happen before `sandbox_dispatch_or_continue`
    // — that call exits in the child process before any further setup.
    if let Some(code) = exarch::install_child_hooks_and_serve_helpers() {
        return std::process::ExitCode::from(code);
    }
    if let Some(code) = exarch::bootstrap::sandbox_dispatch_or_continue() {
        return code;
    }
    match exarch::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("exarch: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

/// Test-binary counterpart to the two startup trampolines at the top of
/// `main` (helper re-exec dispatch, then sandbox `early_init`).  Run from
/// a pre-`main` constructor so both are served before libtest sees the
/// flags either would reject; see [`exarch::serve_test_pre_main`].
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_test_binary() {
    exarch::serve_test_pre_main();
}
