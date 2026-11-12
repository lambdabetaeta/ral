//! Parent-owned context; a failed hook writes only an index and errno.

#[cfg(target_os = "linux")]
use crate::fact::{JailFailure, JailFailureMembership, MembershipStep, OsFailure};
use crate::fact::{
    LaunchFailure, ParcelCause, ParcelFailure, ParcelRole, ParcelStep, PrepareCause,
    PrepareFailure, PrepareStep, PrepareSubject,
};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

pub(crate) enum Context {
    Prepare(PrepareStep, PrepareSubject),
    Parcel(ParcelRole, ParcelStep),
    #[cfg(target_os = "linux")]
    Jail(String, MembershipStep),
}

impl Context {
    fn failure(self, error: &io::Error) -> LaunchFailure {
        match self {
            Self::Prepare(step, subject) => LaunchFailure::Prepare(PrepareFailure {
                step,
                subject,
                cause: PrepareCause::Io(Box::new(crate::io::failure(error))),
            }),
            Self::Parcel(role, step) => LaunchFailure::Parcel(ParcelFailure {
                role,
                step,
                cause: ParcelCause::Io(Box::new(crate::io::failure(error))),
            }),
            #[cfg(target_os = "linux")]
            Self::Jail(cgroup, step) => {
                LaunchFailure::Jail(JailFailure::Membership(JailFailureMembership {
                    cgroup,
                    step,
                    cause: OsFailure::from(error),
                }))
            }
        }
    }
}

pub(crate) struct Evidence {
    reader: UnixStream,
    writer: Arc<UnixStream>,
    contexts: Vec<Context>,
}

pub(crate) struct Check {
    writer: Arc<UnixStream>,
    index: u32,
}

impl Evidence {
    pub(crate) fn new() -> io::Result<Self> {
        let (reader, writer) = crate::process::cloexec_socketpair()?;
        let reader = UnixStream::from(rustix::io::fcntl_dupfd_cloexec(&reader, 3)?);
        let writer = UnixStream::from(rustix::io::fcntl_dupfd_cloexec(&writer, 3)?);
        Ok(Self {
            reader,
            writer: Arc::new(writer),
            contexts: Vec::new(),
        })
    }

    pub(crate) fn check(&mut self, context: Context) -> Check {
        let index =
            u32::try_from(self.contexts.len()).expect("a launch has fewer than 2^32 setup steps");
        self.contexts.push(context);
        Check {
            writer: Arc::clone(&self.writer),
            index,
        }
    }

    /// Nonblocking: std has already received the failed child's errno.
    pub(crate) fn reported_failure(self) -> Option<LaunchFailure> {
        let mut bytes = [0; 8];
        if matches!(
            rustix::io::retry_on_intr(|| rustix::net::recv(
                &self.reader,
                &mut bytes,
                rustix::net::RecvFlags::DONTWAIT
            )),
            Ok((8, _))
        ) {
            let index = u32::from_ne_bytes(bytes[..4].try_into().expect("fixed width"));
            let errno = i32::from_ne_bytes(bytes[4..].try_into().expect("fixed width"));
            return Some(
                self.contexts
                    .into_iter()
                    .nth(index as usize)
                    .expect("the child writes only a parent-prepared step index")
                    .failure(&io::Error::from_raw_os_error(errno)),
            );
        }
        None
    }
}

impl Check {
    /// Post-fork: no allocation, locks, or portable-fact construction.
    pub(crate) fn run<T>(&self, result: io::Result<T>) -> io::Result<T> {
        result.map_err(|error| self.fail(error))
    }

    pub(crate) fn fail(&self, error: io::Error) -> io::Error {
        let mut bytes = [0; 8];
        bytes[..4].copy_from_slice(&self.index.to_ne_bytes());
        bytes[4..].copy_from_slice(
            &error
                .raw_os_error()
                .expect("setup hooks return native errno")
                .to_ne_bytes(),
        );
        // SAFETY: a live owned socket and a fixed stack record. One failed step writes once.
        let mut written = 0;
        while written < bytes.len() {
            let count = unsafe {
                libc::write(
                    self.writer.as_raw_fd(),
                    bytes[written..].as_ptr().cast(),
                    bytes.len() - written,
                )
            };
            if count > 0 {
                written += usize::try_from(count).expect("positive write count");
            } else if count == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
        }
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_reconstructs_the_prepared_step_and_native_errno() {
        let mut evidence = Evidence::new().unwrap();
        let check = evidence.check(Context::Parcel(ParcelRole::Warrant, ParcelStep::Keep));
        check
            .run::<()>(Err(io::Error::from_raw_os_error(libc::EBADF)))
            .unwrap_err();
        assert_eq!(
            evidence.reported_failure().unwrap(),
            LaunchFailure::Parcel(ParcelFailure {
                role: ParcelRole::Warrant,
                step: ParcelStep::Keep,
                cause: ParcelCause::Io(Box::new(crate::io::failure(
                    &io::Error::from_raw_os_error(libc::EBADF)
                ))),
            })
        );
    }
}
