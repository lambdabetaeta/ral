//! The workspace's poison policy, in one place.
//!
//! [`LockExt`] for a `Mutex`, [`RwLockExt`] for an `RwLock`, [`CondvarExt`] for
//! the `Condvar` parked on one. The reasoning is [`LockExt`]'s, and a mutex
//! that does not meet it takes [`lock_strict`](LockExt::lock_strict), which
//! keeps poison; the lock's own doc says what tears.
//!
//! A third spelling is neither this policy nor an exception to it. Where a `Mutex`
//! exists for interior mutability through an `Arc` and one thread by
//! construction is the only one that can hold the guard, `try_lock` with a panic
//! on `WouldBlock` is an *assertion*: contention is impossible, so `WouldBlock`
//! can only mean reentrancy — a bug `lock` would answer with a deadlock and this
//! answers with a diagnosis. Poison never arises there, since there is no second
//! thread to have panicked. `ReplyCell::lock` and `LogCell::lock` in
//! `exarch/src/agent/shell.rs`, and `ActFragment::lock` in
//! `exarch/src/fleet/desk.rs`, are the three; each states the single-holder
//! invariant that earns it in its own doc.

use std::fmt;
use std::io;

mod site;

use self::site::Silent;
use crate::site::site;

/// Lock, recovering the guard if a prior holder panicked.
///
/// Every mutation under a lock reached through this helper is total — a
/// `HashMap`/`VecDeque` entry, an `Option` swap, an `Instant` overwrite — so
/// poison marks an unrelated panic rather than torn data, and propagating it
/// would disable the lock for everyone thereafter rather than the one run that
/// panicked.
///
/// Totality is a question about unwind points, not about arity: can a panic land
/// *between* two of the writes? `Vec::push` aborts rather than unwinds,
/// `saturating_sub` cannot panic, an `extern "system"` call is no unwind point —
/// so an update touching many fields through those is total, while a lone write
/// fed by something fallible may not be.
///
/// A lock whose mutation is *not* total has no business here: it takes
/// [`lock_strict`](Self::lock_strict).  The wire writer is the standing example,
/// since a panic mid-frame leaves a partial frame on the socket and resuming
/// into a torn stream is worse than refusing. Its own doc carries that
/// reasoning.
pub trait LockExt<T> {
    fn lock_ignore_poison(&self) -> std::sync::MutexGuard<'_, T>;

    /// Take the value out, recovering it if a prior holder panicked.
    ///
    /// The argument is stronger here than at `lock`: `into_inner` consumes the
    /// mutex, so the caller is its sole owner and no thread can be holding the
    /// guard. Poison can only record a panic that has already finished; there
    /// is no live writer left to tear anything, and nothing to protect the
    /// value from.
    fn into_inner_ignore_poison(self) -> T;

    /// The other policy, poison kept: for a lock whose guarded state restates
    /// something outside the process — a frame on a socket, a file's length —
    /// so a panic between the outside change and its restatement leaves the two
    /// disagreeing.  The lock's own doc says what tears.
    ///
    /// # Errors
    /// [`Torn`] where a prior holder panicked.
    fn lock_strict(&self) -> Result<std::sync::MutexGuard<'_, T>, Torn>;
}

/// A lock refused because a holder panicked under it: what the state it
/// guards restates outside the process may no longer agree with it.
#[derive(Debug)]
pub struct Torn;

impl fmt::Display for Torn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a thread panicked while holding this lock, so the state it guards may be torn")
    }
}

impl std::error::Error for Torn {}

impl From<Torn> for io::Error {
    fn from(torn: Torn) -> Self {
        Self::other(torn)
    }
}

impl<T> LockExt<T> for std::sync::Mutex<T> {
    fn lock_ignore_poison(&self) -> std::sync::MutexGuard<'_, T> {
        site!(Silent::IgnorePoison, self => lock())
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn into_inner_ignore_poison(self) -> T {
        site!(Silent::IgnorePoison, self => into_inner())
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_strict(&self) -> Result<std::sync::MutexGuard<'_, T>, Torn> {
        site!(Silent::KeepPoison, self => lock()).map_err(|_| Torn)
    }
}

/// The same policy for an `RwLock`, on the same reading of poison.
///
/// A reader cannot tear anything, so `read_ignore_poison` is the only sane
/// spelling; `write_ignore_poison` carries the totality obligation the mutex
/// helper does.
pub trait RwLockExt<T> {
    fn read_ignore_poison(&self) -> std::sync::RwLockReadGuard<'_, T>;
    fn write_ignore_poison(&self) -> std::sync::RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for std::sync::RwLock<T> {
    fn read_ignore_poison(&self) -> std::sync::RwLockReadGuard<'_, T> {
        site!(Silent::IgnorePoison, self => read())
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn write_ignore_poison(&self) -> std::sync::RwLockWriteGuard<'_, T> {
        site!(Silent::IgnorePoison, self => write())
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The same policy for a `Condvar`, which needs a trait of its own.
///
/// `wait` re-acquires the mutex before it returns, so it is a second acquisition
/// of that very lock and observes poison exactly as `lock` does. A policy
/// applied only at `lock` is half-applied — and `wait` is precisely where a
/// thread sits while another panics, so the half left out is the one poison
/// actually reaches.
///
/// Each method returns what `std`'s does minus the `LockResult`; the timeout
/// form keeps its `WaitTimeoutResult`, since whether the wait timed out is the
/// caller's business and not poison's.
pub trait CondvarExt {
    fn wait_ignore_poison<'a, T>(
        &self,
        guard: std::sync::MutexGuard<'a, T>,
    ) -> std::sync::MutexGuard<'a, T>;

    fn wait_timeout_ignore_poison<'a, T>(
        &self,
        guard: std::sync::MutexGuard<'a, T>,
        dur: std::time::Duration,
    ) -> (std::sync::MutexGuard<'a, T>, std::sync::WaitTimeoutResult);

    fn wait_while_ignore_poison<'a, T>(
        &self,
        guard: std::sync::MutexGuard<'a, T>,
        condition: impl FnMut(&mut T) -> bool,
    ) -> std::sync::MutexGuard<'a, T>;
}

impl CondvarExt for std::sync::Condvar {
    fn wait_ignore_poison<'a, T>(
        &self,
        guard: std::sync::MutexGuard<'a, T>,
    ) -> std::sync::MutexGuard<'a, T> {
        site!(Silent::IgnorePoison, self => wait(guard))
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait_timeout_ignore_poison<'a, T>(
        &self,
        guard: std::sync::MutexGuard<'a, T>,
        dur: std::time::Duration,
    ) -> (std::sync::MutexGuard<'a, T>, std::sync::WaitTimeoutResult) {
        site!(Silent::IgnorePoison, self => wait_timeout(guard, dur))
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn wait_while_ignore_poison<'a, T>(
        &self,
        guard: std::sync::MutexGuard<'a, T>,
        condition: impl FnMut(&mut T) -> bool,
    ) -> std::sync::MutexGuard<'a, T> {
        site!(Silent::IgnorePoison, self => wait_while(guard, condition))
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A mutex some thread panicked under, holding its guard.
    fn poisoned() -> Arc<Mutex<u8>> {
        let lock = Arc::new(Mutex::new(1));
        let holder = Arc::clone(&lock);
        let joined = std::thread::spawn(move || {
            let _guard = holder.lock_ignore_poison();
            panic!("a holder panics under the lock");
        })
        .join();
        assert!(joined.is_err());
        lock
    }

    #[test]
    fn strict_keeps_the_poison_that_the_other_policy_recovers() {
        let lock = poisoned();
        assert!(lock.lock_strict().is_err());
        assert_eq!(*lock.lock_ignore_poison(), 1);
    }

    #[test]
    fn strict_locks_a_sound_mutex() {
        let lock = Mutex::new(7);
        assert_eq!(lock.lock_strict().map(|guard| *guard).ok(), Some(7));
    }

    #[test]
    fn torn_reads_as_an_io_error_that_says_so() {
        let e = io::Error::from(Torn);
        assert!(e.to_string().contains("torn"), "{e}");
    }
}
