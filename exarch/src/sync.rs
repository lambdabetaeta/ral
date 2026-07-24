use std::sync::{Mutex, MutexGuard, PoisonError};

/// Lock the mutex, recovering the guard if a prior holder panicked while
/// holding it. Every lock in this crate guards data whose invariants survive
/// a panicked critical section (each mutation is total), so a poisoned lock
/// is not evidence of corruption — propagating it would only turn one
/// unrelated panic into a permanently unusable lock for everyone else.
pub(crate) trait LockExt<T> {
    fn lock_ignore_poison(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_ignore_poison(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
