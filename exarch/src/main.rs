//! The exarch binary — a thin shell over [`exarch::run`], with every decision
//! left in the library crate so integration tests link it directly.

fn main() -> std::process::ExitCode {
    #[cfg(unix)]
    ral_core::uutils::init_signal_dispositions();
    // A helper or sandbox re-exec child is served and exits here, never
    // reaching the CLI; `ral`'s `main` opens the same way.
    if let Some(code) = exarch::dispatch_pre_main() {
        std::process::exit(i32::from(code));
    }
    let code = match exarch::run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("exarch: {e}");
            std::process::ExitCode::from(1)
        }
    };
    // A no-op off Windows, where it reverts this session's grant ACEs.
    ral_core::sandbox::teardown_session();
    code
}

// This target's test binary never runs the `main` above, so the ctor makes
// the same dispatch call before libtest reads argv.
#[cfg(test)]
exarch::pre_main_ctor!();
