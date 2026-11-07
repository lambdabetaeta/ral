//! The record `try` hands its handler, `poll` its `` `err `` payload, and the
//! report envelope its failed outcome.  Bytes are absent by design: `audit` is
//! the forensic path.  Minted from an `Error` in `shell::runtime::error`.

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
    /// What an `io::Error` met launching a program means: a refusal is
    /// permission denied, anything else is the OS's own words.
    pub(crate) fn of_io(e: &std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            _ => Self::Io(e.to_string()),
        }
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

impl Reason {
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
            Self::TimedOut => "worker timed out".into(),
            Self::Cancelled => "worker was cancelled".into(),
            Self::Raised(message) => message.as_str().into(),
        }
    }

    pub fn hint(&self) -> Option<Cow<'_, str>> {
        match self {
            Self::Process(process) => process.hint(),
            _ => None,
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
                let names = |what: &str| {
                    if found == program {
                        format!("{program} is {what}")
                    } else {
                        format!("{program}: {found} is {what}")
                    }
                };
                match why {
                    Unrunnable::PermissionDenied => format!("{program}: permission denied"),
                    Unrunnable::Directory => names("a directory, not a command"),
                    Unrunnable::NotExecutable => names("a file, not an executable"),
                    Unrunnable::MissingInterpreter(Some(interpreter)) => {
                        format!("{program}: its interpreter {interpreter} does not exist")
                    }
                    Unrunnable::MissingInterpreter(None) => {
                        format!("{program}: the interpreter or loader it names does not exist")
                    }
                    Unrunnable::Io(message) => format!("{program}: {message}"),
                }
            }
        }
    }

    fn hint(&self) -> Option<Cow<'_, str>> {
        match self.failure {
            #[cfg(unix)]
            Failure::Signaled(libc::SIGKILL) => Some(
                "the process was killed with SIGKILL; the kernel or another process may have terminated it".into(),
            ),
            #[cfg(unix)]
            Failure::Signaled(libc::SIGSEGV) => {
                Some("the process crashed with a segmentation fault".into())
            }
            Failure::Signaled(n) => Some(format!(
                "the process terminated from {}", crate::process::Signal::new(n)
            ).into()),
            _ => None,
        }
    }
}

impl From<crate::process::WorkerStop> for Reason {
    fn from(stop: crate::process::WorkerStop) -> Self {
        match stop {
            crate::process::WorkerStop::Cancelled => Self::Cancelled,
            crate::process::WorkerStop::TimedOut => Self::TimedOut,
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
