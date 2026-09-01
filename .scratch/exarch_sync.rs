use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock, recovering the guard if a prior holder panicked. Every mutation under
/// these locks is total, so poison marks an unrelated panic rather than corrupt
/// data, and propagating it would disable the lock for everyone thereafter.
pub(crate) trait LockExt<T> {
    fn lock_ignore_poison(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_ignore_poison(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
