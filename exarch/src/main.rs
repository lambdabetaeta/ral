//! Exarch binary entry point — a thin shell over [`exarch::run`].  All
//! the agent lives in the library crate so integration tests can link
//! it directly; `main` only wires the pre-`main` helper trampoline and
//! the sandbox dispatch that must run before the frontend.

fn main() -> std::process::ExitCode {
    #[cfg(unix)]
    ral_core::builtins::uutils::init_signal_dispositions();
    // Serve any helper / sandbox re-exec in one call: dress a sandbox-IPC
    // child's fresh shell with exarch's host builtins, serve the
    // pipeline-stage / test helper dispatch, then the OS-sandbox stage.
    // A re-exec child exits here before any further setup.
    if let Some(code) = exarch::dispatch_pre_main() {
        std::process::exit(i32::from(code));
    }
    match exarch::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("exarch: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

/// Test-binary counterpart to the pre-`main` re-exec dispatch at the top
/// of `main`.  Run from a pre-`main` constructor so every helper / sandbox
/// re-exec is served before libtest sees the flags it would reject; see
/// [`exarch::dispatch_pre_main`].
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_test_binary() {
    if let Some(code) = exarch::dispatch_pre_main() {
        std::process::exit(i32::from(code));
    }
}
