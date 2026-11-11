//! Whether the OS enforces a grant: each test needs a confined launch to mean anything.

mod fs;
mod ipc;
mod landlock;
mod loader_env;
mod probe;
mod survivors;
mod warrants;

/// Fixtures that must run before `serve_process_before_main`.
pub(crate) fn before_serve() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    warrants::hold();
}

/// Fixtures that run after it, as a re-exec role is served and the shell booted first.
pub(crate) fn after_serve() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    warrants::fixture();
    #[cfg(all(feature = "coreutils", target_os = "macos"))]
    loader_env::fixture();
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    survivors::fixture();
}
