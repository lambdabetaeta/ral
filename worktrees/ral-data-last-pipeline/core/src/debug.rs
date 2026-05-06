//! Debug tracing macros.
//!
//! Two macros for stderr diagnostics during development:
//!
//! - [`debug_trace!`] -- gated by the `AL_DEBUG` environment variable.
//!   Compiled away entirely in release builds.
//! - [`dbg_trace!`] -- always prints in debug builds (no shell check),
//!   compiled away in release.  Takes a tag for filtering by subsystem.
//!
//! Neither macro should ever be removed from the source; they are
//! permanent instrumentation, not temporary print statements.

/// Conditional debug trace.  Prints `[debug] ...` to stderr when
/// `AL_DEBUG` is set in the environment.  Compiled to nothing in release.
///
/// ```ignore
/// debug_trace!("entering eval_node: {:?}", node);
/// ```
#[cfg(debug_assertions)]
#[macro_export]
macro_rules! debug_trace {
    ($($arg:tt)*) => {
        if std::env::var("AL_DEBUG").is_ok() {
            eprintln!("[debug] {}", format!($($arg)*))
        }
    }
}

#[cfg(not(debug_assertions))]
#[macro_export]
macro_rules! debug_trace {
    ($($arg:tt)*) => {};
}
