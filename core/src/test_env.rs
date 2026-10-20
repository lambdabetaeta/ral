//! Serialised process-environment mutation for tests.
//!
//! The environment is process-global and tests run concurrently, so every
//! test that sets or clears `HOME` / `XDG_*_HOME` must take [`env_guard`]
//! first.  The guard is poison-tolerant: a test that panics while holding
//! it must not wedge the rest of the suite.

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard};

use crate::sync::LockExt as _;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Hold the guard across any `set_var` / `remove_var` *and* the reads that
/// depend on it.
pub(crate) fn env_guard() -> MutexGuard<'static, ()> {
    ENV_LOCK.lock_ignore_poison()
}

/// Runs `f` under [`env_guard`] with `key` set to `val` (`None` removes it),
/// restoring the prior value after.
#[allow(
    clippy::disallowed_methods,
    reason = "[test] serialised env mutation for tests, guarded by env_guard"
)]
pub(crate) fn with_var<R>(key: &str, val: Option<&str>, f: impl FnOnce() -> R) -> R {
    let _lock = env_guard();
    // Declared after `_lock`, so it drops (restores) first, before the lock is released.
    let _restore = Restore(vec![(key.to_owned(), std::env::var_os(key))]);
    set_or_remove(key, val);
    f()
}

/// Runs `f` under [`env_guard`] with every var in `keys` removed, restoring them after.
// Unix-gated with its callers: the XDG defaults it clears are asserted only there.
#[cfg(unix)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] serialised env mutation for tests, guarded by env_guard"
)]
pub(crate) fn with_vars_cleared<R>(keys: &[&str], f: impl FnOnce() -> R) -> R {
    let _lock = env_guard();
    // Declared after `_lock`, so it drops (restores) first, before the lock is released.
    let _restore = Restore(
        keys.iter()
            .map(|k| ((*k).to_owned(), std::env::var_os(k)))
            .collect(),
    );
    for k in keys {
        set_or_remove(k, None);
    }
    f()
}

/// Restores the saved variables on drop, so a panicking `f` still restores them.
struct Restore(Vec<(String, Option<OsString>)>);

impl Drop for Restore {
    fn drop(&mut self) {
        for (k, prev) in self.0.drain(..) {
            restore(&k, prev);
        }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "[test] serialised env mutation for tests, guarded by env_guard"
)]
fn set_or_remove(key: &str, val: Option<&str>) {
    match val {
        Some(v) => unsafe { std::env::set_var(key, v) },
        None => unsafe { std::env::remove_var(key) },
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "[test] serialised env mutation for tests, guarded by env_guard"
)]
fn restore(key: &str, prev: Option<OsString>) {
    match prev {
        Some(v) => unsafe { std::env::set_var(key, v) },
        None => unsafe { std::env::remove_var(key) },
    }
}
