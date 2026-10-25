//! Ownership of buffers allocated by Win32's local allocator.

use windows_sys::Win32::Foundation::LocalFree;

pub(super) struct Local<T>(*mut T);

impl<T> Local<T> {
    /// # Safety
    /// `ptr` is null or uniquely owned and released by `LocalFree`.
    pub(super) unsafe fn from_raw(ptr: *mut T) -> Self {
        Self(ptr)
    }

    pub(super) fn as_ptr(&self) -> *mut T {
        self.0
    }
}

impl<T> Drop for Local<T> {
    fn drop(&mut self) {
        // SAFETY: the constructor transfers unique ownership; LocalFree accepts null.
        unsafe { LocalFree(self.0.cast()) };
    }
}
