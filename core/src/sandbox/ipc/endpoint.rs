//! The child-side IPC endpoint, confined at construction.
//!
//! [`IpcEndpoint`] owns the half of the transport that crosses into the
//! re-exec child.  Its single invariant: the OS resource is CLOEXEC-set
//! (Unix) / non-inheritable (Windows) at every moment the endpoint is
//! reachable as a value.  Inheritance is opened only inside
//! [`IpcEndpoint::lend_for_spawn`], for the extent of one spawn, and
//! re-closed before that scope returns — by a private RAII guard, so the
//! close fires on every exit path rather than a trailing statement a
//! reviewer must check.
//!
//! The endpoint is born scrubbed: `adopt` re-arms CLOEXEC / clears
//! inheritance the instant it takes ownership, so no code path observes a
//! long-lived inheritable endpoint.

#[cfg(unix)]
pub(in crate::sandbox) use unix::IpcEndpoint;
#[cfg(windows)]
pub(in crate::sandbox) use windows::IpcEndpoint;

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
    use std::os::unix::net::UnixStream;

    /// The child half of the IPC socketpair, CLOEXEC-set for as long as it
    /// is held.
    pub(in crate::sandbox) struct IpcEndpoint(UnixStream);

    impl IpcEndpoint {
        /// Adopt a raw fd as the endpoint, re-arming `FD_CLOEXEC` so the
        /// value is never reachable while inheritable.
        ///
        /// # Safety
        /// `fd` must be an owned socket fd this process is the sole owner
        /// of; it becomes owned by the returned endpoint.
        pub(in crate::sandbox) unsafe fn adopt(fd: RawFd) -> io::Result<Self> {
            let stream = unsafe { UnixStream::from_raw_fd(fd) };
            set_cloexec(stream.as_raw_fd(), true)?;
            Ok(Self(stream))
        }

        /// Run `f` with the fd temporarily CLOEXEC-cleared so a spawn can
        /// inherit it, restoring CLOEXEC before return on every path.
        pub(in crate::sandbox) fn lend_for_spawn<T>(
            &self,
            f: impl FnOnce(RawFd) -> T,
        ) -> io::Result<T> {
            let fd = self.0.as_raw_fd();
            set_cloexec(fd, false)?;
            let _guard = LendGuard(fd);
            Ok(f(fd))
        }

        /// Borrow the duplex stream for the child's serve loop.  `try_clone`
        /// dups the fd CLOEXEC-set, so the read/write halves keep the
        /// endpoint's invariant.
        pub(in crate::sandbox::ipc) fn streams(&self) -> io::Result<(UnixStream, UnixStream)> {
            Ok((self.0.try_clone()?, self.0.try_clone()?))
        }
    }

    /// Restores `FD_CLOEXEC` on the lent fd when the lend scope ends.
    struct LendGuard(RawFd);

    impl Drop for LendGuard {
        fn drop(&mut self) {
            let _ = set_cloexec(self.0, true);
        }
    }

    /// Set or clear `FD_CLOEXEC` on `fd`.
    fn set_cloexec(fd: RawFd, on: bool) -> io::Result<()> {
        // Safety: fcntl with F_GETFD/F_SETFD on a valid owned fd is a
        // narrow straight-line op touching only that fd's flags.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            let next = if on {
                flags | libc::FD_CLOEXEC
            } else {
                flags & !libc::FD_CLOEXEC
            };
            if libc::fcntl(fd, libc::F_SETFD, next) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
mod windows {
    use super::super::transport_windows::PipeIo;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
    use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};

    /// The client half of the IPC named pipe, non-inheritable for as long
    /// as it is held.
    pub(in crate::sandbox) struct IpcEndpoint(OwnedHandle);

    impl IpcEndpoint {
        /// Adopt a raw handle as the endpoint, clearing
        /// `HANDLE_FLAG_INHERIT` so the value is never reachable while
        /// inheritable.
        ///
        /// # Safety
        /// `raw` must be an owned handle this process is the sole owner of;
        /// it becomes owned by the returned endpoint.
        pub(in crate::sandbox) unsafe fn adopt(raw: RawHandle) -> io::Result<Self> {
            let owned = unsafe { OwnedHandle::from_raw_handle(raw) };
            set_inherit(owned.as_raw_handle() as HANDLE, false)?;
            Ok(Self(owned))
        }

        /// Run `f` with the handle temporarily inheritable so a spawn can
        /// pass it to the child, clearing inheritance before return on
        /// every path.
        pub(in crate::sandbox) fn lend_for_spawn<T>(
            &self,
            f: impl FnOnce(RawHandle) -> T,
        ) -> io::Result<T> {
            let raw = self.0.as_raw_handle();
            set_inherit(raw as HANDLE, true)?;
            let _guard = LendGuard(raw as HANDLE);
            Ok(f(raw))
        }

        /// Borrow the pipe for the child's serve loop.  The adapter is
        /// `Copy`, so the same non-inheritable handle backs both halves.
        pub(in crate::sandbox::ipc) fn pipe_io(&self) -> PipeIo {
            PipeIo {
                handle: self.0.as_raw_handle() as HANDLE,
            }
        }
    }

    /// Clears `HANDLE_FLAG_INHERIT` on the lent handle when the lend scope
    /// ends.
    struct LendGuard(HANDLE);

    impl Drop for LendGuard {
        fn drop(&mut self) {
            let _ = set_inherit(self.0, false);
        }
    }

    /// Set or clear `HANDLE_FLAG_INHERIT` on `handle`.
    fn set_inherit(handle: HANDLE, on: bool) -> io::Result<()> {
        let value = if on { HANDLE_FLAG_INHERIT } else { 0 };
        let ok = unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, value) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
