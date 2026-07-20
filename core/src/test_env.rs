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

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Acquire the shared environment lock.  Hold the returned guard for the
/// duration of any `set_var` / `remove_var` and the reads that depend on it.
pub(crate) fn env_guard() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Run `f` with `key` set to `val` (or removed when `None`), restoring its
/// prior value after — holding [`env_guard`] for the whole scope.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] serialised env mutation for tests, guarded by env_guard"
)]
pub(crate) fn with_var<R>(key: &str, val: Option<&str>, f: impl FnOnce() -> R) -> R {
    let _guard = env_guard();
    let prev = std::env::var_os(key);
    set_or_remove(key, val);
    let out = f();
    restore(key, prev);
    out
}

/// Run `f` with every var in `keys` removed, restoring their prior values
/// after — holding [`env_guard`] for the whole scope.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] serialised env mutation for tests, guarded by env_guard"
)]
pub(crate) fn with_vars_cleared<R>(keys: &[&str], f: impl FnOnce() -> R) -> R {
    let _guard = env_guard();
    let saved: Vec<(&str, Option<OsString>)> =
        keys.iter().map(|k| (*k, std::env::var_os(k))).collect();
    for k in keys {
        set_or_remove(k, None);
    }
    let out = f();
    for (k, prev) in saved {
        restore(k, prev);
    }
    out
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] serialised env mutation for tests, guarded by env_guard"
)]
fn set_or_remove(key: &str, val: Option<&str>) {
    match val {
        Some(v) => unsafe { std::env::set_var(key, v) },
        None => unsafe { std::env::remove_var(key) },
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] serialised env mutation for tests, guarded by env_guard"
)]
fn restore(key: &str, prev: Option<OsString>) {
    match prev {
        Some(v) => unsafe { std::env::set_var(key, v) },
        None => unsafe { std::env::remove_var(key) },
    }
}
