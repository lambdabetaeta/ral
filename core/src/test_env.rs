//! Serialised process-environment mutation for tests.
//!
//! `xdg:` and `~` resolution read `std::env`, and `RUST_TEST_THREADS > 1`
//! (set in `.cargo/config.toml`) runs the lib's unit tests concurrently,
//! so two tests that set or clear `HOME` / `XDG_*_HOME` at the same time
//! corrupt each other's view.  Every env-mutating test in this crate takes
//! the same [`env_guard`] first, making the mutation effectively serial
//! within the test binary.  The guard is poison-tolerant: a test that
//! panics mid-assertion while holding it must not wedge the rest of the
//! suite.

use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the shared environment lock.  Hold the returned guard for the
/// duration of any `set_var` / `remove_var` and the reads that depend on it.
pub(crate) fn env_guard() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
