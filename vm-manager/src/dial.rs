//! Guest socket failures retain native evidence until the caller projects or renders it.

use std::fmt;
use std::io;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialBackend {
    Broker,
    HyperV,
    Virtualization,
}

impl fmt::Display for DialBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Broker => "broker",
            Self::HyperV => "Hyper-V",
            Self::Virtualization => "Virtualization.framework",
        })
    }
}

#[derive(Debug)]
pub enum DialError {
    Unsupported(DialBackend),
    MachineThreadStopped,
    MissingSocketDevice,
    NonVirtioSocketDevice,
    CompletionWithoutResult,
    TimedOut {
        port: u32,
        within: Duration,
    },
    WaitDisconnected {
        port: u32,
    },
    Platform {
        backend: DialBackend,
        domain: String,
        code: i64,
        detail: String,
    },
    Io(io::Error),
}

impl fmt::Display for DialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(backend) => write!(
                f,
                "the {backend} backend does not support connecting to a guest socket"
            ),
            Self::MachineThreadStopped => f.write_str(
                "the machine thread stopped before the guest connection could be obtained",
            ),
            Self::MissingSocketDevice => f.write_str("the machine has no virtio socket device"),
            Self::NonVirtioSocketDevice => f.write_str("the machine's socket device is not virtio"),
            Self::CompletionWithoutResult => {
                f.write_str("the guest connection completed with neither a connection nor an error")
            }
            Self::TimedOut { port, within } => write!(
                f,
                "no guest connection result arrived for port {port} within {within:?}"
            ),
            Self::WaitDisconnected { port } => write!(
                f,
                "the guest connection result channel disconnected while waiting for port {port}"
            ),
            Self::Platform {
                backend,
                domain,
                code,
                detail,
            } => write!(
                f,
                "{backend} refused the guest connection: {domain} error {code}: {detail}"
            ),
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for DialError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn dial_error_retains_the_native_io_source() {
        let native = io::Error::from_raw_os_error(9);
        let wording = native.to_string();
        let error = DialError::Io(native);
        assert_eq!(error.to_string(), wording);
        assert_eq!(
            error
                .source()
                .and_then(|source| source.downcast_ref::<io::Error>())
                .and_then(io::Error::raw_os_error),
            Some(9)
        );
    }
}
