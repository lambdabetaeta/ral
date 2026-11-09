//! The record `try` hands its handler, `poll` its `` `err `` payload, and the
//! report envelope its failed outcome.  Bytes are absent by design: `audit` is
//! the forensic path.  Minted from an `Error` in `shell::runtime::error`.

use crate::process::CancelCause;
use crate::source::CallSite;
use crate::{record, variant};
use std::borrow::Cow;

/// Why a failure happened, as the tagged value `$err[reason]` reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    Process(Box<Process>),
    TimedOut,
    Cancelled,
    Raised(String),
}

variant!(typed Reason {
    Process(Box<Process>): "process",
    TimedOut: "timed-out",
    Cancelled: "cancelled",
    Raised(String): "raised",
});

/// A program that did not succeed: its head as dispatched, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub program: String,
    pub failure: Failure,
}

record!(typed Process {
    program: "program",
    failure: "failure",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Exited(i32),
    Signaled(i32),
    NotFound,
    NotRunnable(NotRunnable),
}

variant!(typed Failure {
    Exited(i32): "exited",
    Signaled(i32): "signaled",
    NotFound: "not-found",
    NotRunnable(NotRunnable): "not-runnable",
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotRunnable {
    pub found: String,
    pub why: Unrunnable,
}

record!(typed NotRunnable { found: "found", why: "why" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unrunnable {
    PermissionDenied,
    Directory,
    NotExecutable,
    /// The `#!` interpreter the file names, when it names one.
    MissingInterpreter(Option<String>),
    Io(String),
}

variant!(typed Unrunnable {
    PermissionDenied: "permission-denied",
    Directory: "directory",
    NotExecutable: "not-executable",
    MissingInterpreter(Option<String>): "missing-interpreter",
    Io(String): "io",
});

impl Unrunnable {
    /// Fallback after lookup or startup has interpreted context-dependent
    /// failures such as `NotFound`: permission denied is structured, anything
    /// else retains the OS's own words.
    pub(crate) fn from_io(error: &std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            _ => Self::Io(error.to_string()),
        }
    }
}

impl Failure {
    /// `found`, where the program was sought, cannot run for `why`.
    pub(crate) fn not_runnable(found: impl Into<String>, why: Unrunnable) -> Self {
        Self::NotRunnable(NotRunnable {
            found: found.into(),
            why,
        })
    }
}

/// The failure value: why, the frame it is reported at, and its location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorRecord {
    pub(crate) frame: String,
    pub(crate) reason: Reason,
    pub(crate) site: CallSite,
}

record!(typed ErrorRecord {
    frame: "frame",
    reason: "reason",
    site: "site",
});

impl From<String> for Reason {
    fn from(message: String) -> Self {
        Self::Raised(message)
    }
}

impl From<&str> for Reason {
    fn from(message: &str) -> Self {
        Self::Raised(message.to_owned())
    }
}

impl Reason {
    /// The reason a worker's own switch records for `cause`: a cancel or a
    /// timeout, the two a worker converts; any other stop is the host's.
    pub fn stopped(cause: CancelCause) -> Option<Self> {
        match cause {
            CancelCause::Cancelled => Some(Self::Cancelled),
            CancelCause::TimedOut => Some(Self::TimedOut),
            _ => None,
        }
    }

    pub fn process(program: impl Into<String>, failure: Failure) -> Self {
        Self::Process(Box::new(Process {
            program: program.into(),
            failure,
        }))
    }

    pub fn failure(&self) -> Option<&Failure> {
        match self {
            Self::Process(process) => Some(&process.failure),
            _ => None,
        }
    }

    pub fn describe(&self) -> Cow<'_, str> {
        match self {
            Self::Process(process) => process.describe().into(),
            Self::TimedOut => CancelCause::TimedOut.message().into(),
            Self::Cancelled => CancelCause::Cancelled.message().into(),
            Self::Raised(message) => message.as_str().into(),
        }
    }
}

impl Process {
    fn describe(&self) -> String {
        let Self { program, failure } = self;
        match failure {
            #[cfg(windows)]
            Failure::Exited(windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT) => {
                format!("{program}: ended by Ctrl-C")
            }
            Failure::Exited(n) => format!("{program}: exited with status {n}"),
            Failure::Signaled(n) => format!(
                "{program}: killed by signal {}",
                crate::process::Signal::new(*n)
            ),
            Failure::NotFound => format!("{program}: command not found"),
            Failure::NotRunnable(NotRunnable { found, why }) => {
                let at = if found == program {
                    program.clone()
                } else {
                    format!("{program}: {found}")
                };
                match why {
                    Unrunnable::PermissionDenied => format!("{at}: permission denied"),
                    Unrunnable::Directory => format!("{at} is a directory, not a command"),
                    Unrunnable::NotExecutable => format!("{at} is a file, not an executable"),
                    Unrunnable::MissingInterpreter(Some(interpreter)) => {
                        format!("{at}: its interpreter {interpreter} does not exist")
                    }
                    Unrunnable::MissingInterpreter(None) => {
                        format!("{at}: the interpreter or loader it names does not exist")
                    }
                    Unrunnable::Io(message) => format!("{at}: {message}"),
                }
            }
        }
    }
}

impl serde::Serialize for ErrorRecord {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        crate::data::datum::as_datum::serialize(self, s)
    }
}

impl<'de> serde::Deserialize<'de> for ErrorRecord {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::data::datum::as_datum::deserialize(d)
    }
}

impl ErrorRecord {
    pub fn new(reason: Reason, frame: String, site: CallSite) -> Self {
        Self {
            frame,
            reason,
            site,
        }
    }

    pub fn frame(&self) -> &str {
        &self.frame
    }

    pub fn reason(&self) -> &Reason {
        &self.reason
    }

    pub fn describe(&self) -> Cow<'_, str> {
        self.reason.describe()
    }

    pub fn site(&self) -> &CallSite {
        &self.site
    }
}
