//! The crate's poison policy, in one door.

/// Lock, recovering the guard if a prior holder panicked. Every mutation under
/// a lock reached through this door is total — a `HashMap`/`VecDeque` entry, an
/// `Option` swap, an `Instant` overwrite — so poison marks an unrelated panic
/// rather than torn data, and propagating it would disable the lock for
/// everyone thereafter rather than the one run that panicked.
///
/// A lock whose mutation is *not* total has no business here: the wire writer
/// is the standing exception, since a panic mid-frame leaves a partial frame on
/// the socket and resuming into a torn stream is worse than refusing. Its own
/// doc carries that reasoning.
pub(crate) trait LockExt<T> {
    fn lock_ignore_poison(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockExt<T> for std::sync::Mutex<T> {
    fn lock_ignore_poison(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
