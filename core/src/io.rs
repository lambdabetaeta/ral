//! Shell byte streams and their terminal presentation.
//!
//! [`Io`] bundles a `Shell`'s three streams. Stream plumbing sits over [`edge`] / [`source`] / [`sink`]; [`terminal`] gates
//! terminal output, using the escape vocabulary in [`ansi`].

pub mod ansi;
mod bounded;
pub(crate) mod child;
mod edge;
pub(crate) mod frame;
mod poll;
mod regime;
mod sink;
mod source;
mod state;
mod streams;
pub mod terminal;
pub use bounded::read_bounded;
pub(crate) use edge::Edge;
pub use frame::{FrameDump, FrameReadError};
#[cfg(unix)]
pub(crate) use poll::poll_slice;
#[cfg(windows)]
pub(crate) use poll::{aborted, interrupt};
pub use regime::{RunIo, RunStdin};
pub use sink::{ByteBuffer, Captured, CapturedBytes, LineFn, Retained, Sink, without_terminator};
pub(crate) use sink::{SINK_BUFFER_CAP, WriteFault, new_buffer, tee_with_buffer};
pub use source::{Source, SourceReader};
pub use streams::Streams;

pub(crate) use state::{Io, IoPatch};

use crate::fact::{IoCause, IoKind, IoLeaf, OsFailure, Unrunnable};
use std::{fmt, io};

/// Extract local causes before the standard library's OS classification.
pub fn failure(error: &io::Error) -> IoCause {
    if let Some(cause) = error.get_ref().and_then(|e| e.downcast_ref::<IoCause>()) {
        return cause.clone();
    }
    if let Some(cause) = crate::path::io_failure(error) {
        return cause;
    }
    if let Some(cause) = error
        .get_ref()
        .and_then(|e| e.downcast_ref::<crate::own::OwnError>())
    {
        return cause.into();
    }
    if error
        .get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<crate::sync::Torn>)
    {
        return IoCause::new(IoLeaf::Torn);
    }
    if let Some(cause) = error
        .get_ref()
        .and_then(|e| e.downcast_ref::<crate::fact::JsonEncodeFailure>())
    {
        return IoCause::new(IoLeaf::JsonEncoding(cause.clone()));
    }
    IoCause::new(IoLeaf::Os(error.into()))
}

impl From<io::ErrorKind> for IoKind {
    fn from(kind: io::ErrorKind) -> Self {
        match kind {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::ConnectionRefused => Self::ConnectionRefused,
            io::ErrorKind::ConnectionReset => Self::ConnectionReset,
            io::ErrorKind::HostUnreachable => Self::HostUnreachable,
            io::ErrorKind::NetworkUnreachable => Self::NetworkUnreachable,
            io::ErrorKind::ConnectionAborted => Self::ConnectionAborted,
            io::ErrorKind::NotConnected => Self::NotConnected,
            io::ErrorKind::AddrInUse => Self::AddrInUse,
            io::ErrorKind::AddrNotAvailable => Self::AddrNotAvailable,
            io::ErrorKind::NetworkDown => Self::NetworkDown,
            io::ErrorKind::BrokenPipe => Self::BrokenPipe,
            io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            io::ErrorKind::WouldBlock => Self::WouldBlock,
            io::ErrorKind::NotADirectory => Self::NotADirectory,
            io::ErrorKind::IsADirectory => Self::IsADirectory,
            io::ErrorKind::DirectoryNotEmpty => Self::DirectoryNotEmpty,
            io::ErrorKind::ReadOnlyFilesystem => Self::ReadOnlyFilesystem,
            io::ErrorKind::StaleNetworkFileHandle => Self::StaleNetworkFileHandle,
            io::ErrorKind::InvalidInput => Self::InvalidInput,
            io::ErrorKind::InvalidData => Self::InvalidData,
            io::ErrorKind::TimedOut => Self::TimedOut,
            io::ErrorKind::WriteZero => Self::WriteZero,
            io::ErrorKind::StorageFull => Self::StorageFull,
            io::ErrorKind::NotSeekable => Self::NotSeekable,
            io::ErrorKind::QuotaExceeded => Self::QuotaExceeded,
            io::ErrorKind::FileTooLarge => Self::FileTooLarge,
            io::ErrorKind::ResourceBusy => Self::ResourceBusy,
            io::ErrorKind::ExecutableFileBusy => Self::ExecutableFileBusy,
            io::ErrorKind::Deadlock => Self::Deadlock,
            io::ErrorKind::CrossesDevices => Self::CrossesDevices,
            io::ErrorKind::TooManyLinks => Self::TooManyLinks,
            io::ErrorKind::InvalidFilename => Self::InvalidFilename,
            io::ErrorKind::ArgumentListTooLong => Self::ArgumentListTooLong,
            io::ErrorKind::Interrupted => Self::Interrupted,
            io::ErrorKind::Unsupported => Self::Unsupported,
            io::ErrorKind::UnexpectedEof => Self::UnexpectedEof,
            io::ErrorKind::OutOfMemory => Self::OutOfMemory,
            io::ErrorKind::Other => Self::Other,
            _ => Self::Unclassified,
        }
    }
}

impl From<&io::Error> for OsFailure {
    fn from(error: &io::Error) -> Self {
        Self {
            kind: error.kind().into(),
            raw: error.raw_os_error(),
            detail: Some(error.to_string()),
        }
    }
}

impl fmt::Display for IoCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

impl std::error::Error for IoCause {}

impl Unrunnable {
    pub(crate) fn from_io(error: &io::Error) -> Self {
        if error.kind() == io::ErrorKind::PermissionDenied {
            Self::PermissionDenied
        } else {
            Self::Io(Box::new(failure(error)))
        }
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    use crate::fact::{IoContext, PathIoContext, PathIoLeaf};

    #[test]
    fn custom_path_causes_are_extracted_before_the_permission_kind() {
        let error = crate::path::directory_refusal_for_test();
        assert_eq!(
            failure(&error),
            IoCause::new(IoLeaf::Path(PathIoLeaf::DirectoryNotWritable))
        );
        assert_eq!(Unrunnable::from_io(&error), Unrunnable::PermissionDenied);
    }

    #[test]
    fn os_projection_retains_raw_code_and_fixed_not_found_wording() {
        let error = io::Error::from(io::ErrorKind::NotFound);
        let IoLeaf::Os(cause) = *failure(&error).leaf else {
            panic!("OS leaf")
        };
        assert_eq!(cause.kind, IoKind::NotFound);
        assert_eq!(cause.describe(), "no such file or directory");
        let raw = io::Error::from_raw_os_error(12345);
        let cause = OsFailure::from(&raw);
        assert_eq!(cause.raw, Some(12345));
    }

    #[test]
    fn typed_spines_survive_borrowed_listing_error_copies() {
        let cause = IoCause::new(IoLeaf::Path(PathIoLeaf::NotHeld))
            .at(IoContext::Path(PathIoContext::UnsafeScopedInheritance));
        let source = io::Error::other(cause.clone());
        let copied = io::Error::new(source.kind(), failure(&source));
        assert_eq!(failure(&copied), cause);
        assert_eq!(
            Unrunnable::from_io(&copied),
            Unrunnable::Io(Box::new(cause))
        );
    }

    #[test]
    fn torn_and_json_encoding_remain_distinct_local_leaves() {
        assert_eq!(
            failure(&io::Error::from(crate::sync::Torn)),
            IoCause::new(IoLeaf::Torn)
        );
        let cause =
            crate::fact::JsonEncodeFailure::new(crate::fact::JsonEncodeCause::NonUnicodePath(
                crate::fact::JsonEncodeCauseNonUnicodePath {
                    shown: "file".into(),
                },
            ));
        assert_eq!(
            failure(&io::Error::other(cause.clone())),
            IoCause::new(IoLeaf::JsonEncoding(cause))
        );
    }

    #[cfg(windows)]
    #[test]
    fn write_abort_uncertainty_preserves_the_underlying_os_evidence() {
        let error = io::Error::from_raw_os_error(995);
        let cause = IoCause::new(IoLeaf::WriteAbortedUnknownPrefix((&error).into()));
        assert_eq!(failure(&io::Error::other(cause.clone())), cause);
    }
}
