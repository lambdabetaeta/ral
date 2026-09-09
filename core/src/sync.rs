//! The workspace's poison policy, in one door.
//!
//! [`LockExt`] for a `Mutex`, [`RwLockExt`] for an `RwLock`, [`CondvarExt`] for
//! the `Condvar` parked on one. The reasoning is [`LockExt`]'s, and a lock that
//! does not meet it stays on `std`'s propagating spelling with a written reason
//! at the call site.
//!
//! A third spelling is neither this door nor an exception to it. Where a `Mutex`
//! exists for interior mutability through an `Arc` and one thread by
//! construction is the only one that can hold the guard, `try_lock` with a panic
//! on `WouldBlock` is an *assertion*: contention is impossible, so `WouldBlock`
//! can only mean reentrancy — a bug `lock` would answer with a deadlock and this
//! answers with a diagnosis. Poison never arises there, since there is no second
//! thread to have panicked. `ReplyCell::lock` and `LogCell::lock` in
//! `exarch/src/agent/shell.rs`, and `ActFragment::lock` in
//! `exarch/src/fleet/desk.rs`, are the three; each states the single-holder
//! invariant that earns it in its own doc.

/// Lock, recovering the guard if a prior holder panicked.
///
/// Every mutation under a lock reached through this door is total — a
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
/// A lock whose mutation is *not* total has no business here: the wire writer
/// is the standing exception, since a panic mid-frame leaves a partial frame on
/// the socket and resuming into a torn stream is worse than refusing. Its own
/// doc carries that reasoning.
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
}

impl<T> LockExt<T> for std::sync::Mutex<T> {
    #[allow(clippy::disallowed_methods, reason = "the door itself")]
    fn lock_ignore_poison(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[expect(clippy::disallowed_methods, reason = "the recovery itself")]
    fn into_inner_ignore_poison(self) -> T {
        self.into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The same door for an `RwLock`, on the same reading of poison.
///
/// A reader cannot tear anything, so `read_ignore_poison` is the only sane
/// spelling; `write_ignore_poison` carries the totality obligation the mutex
/// door does.
pub trait RwLockExt<T> {
    fn read_ignore_poison(&self) -> std::sync::RwLockReadGuard<'_, T>;
    fn write_ignore_poison(&self) -> std::sync::RwLockWriteGuard<'_, T>;
}

impl<T> RwLockExt<T> for std::sync::RwLock<T> {
    #[allow(clippy::disallowed_methods, reason = "the door itself")]
    fn read_ignore_poison(&self) -> std::sync::RwLockReadGuard<'_, T> {
        self.read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[allow(clippy::disallowed_methods, reason = "the door itself")]
    fn write_ignore_poison(&self) -> std::sync::RwLockWriteGuard<'_, T> {
        self.write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The same door for a `Condvar`, which needs one of its own.
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
}

impl CondvarExt for std::sync::Condvar {
    #[allow(clippy::disallowed_methods, reason = "the door itself")]
    fn wait_ignore_poison<'a, T>(
        &self,
        guard: std::sync::MutexGuard<'a, T>,
    ) -> std::sync::MutexGuard<'a, T> {
        self.wait(guard)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[allow(clippy::disallowed_methods, reason = "the door itself")]
    fn wait_timeout_ignore_poison<'a, T>(
        &self,
        guard: std::sync::MutexGuard<'a, T>,
        dur: std::time::Duration,
    ) -> (std::sync::MutexGuard<'a, T>, std::sync::WaitTimeoutResult) {
        self.wait_timeout(guard, dur)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
