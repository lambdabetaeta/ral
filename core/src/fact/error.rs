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
    Io(String),
}

variant!(typed Unrunnable {
    PermissionDenied: "permission-denied",
    Directory: "directory",
    NotExecutable: "not-executable",
    Io(String): "io",
});

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

    pub(crate) fn from_command(program: &str, failure: crate::process::CommandFailure) -> Self {
        use crate::process::CommandFailure as C;
        let failure = match failure {
            C::ExitCode(n) => Failure::Exited(n),
            C::Signal(s) => Failure::Signaled(s.number()),
            C::Spawn(f) => Failure::from_spawn(program, f),
        };
        Self::process(program, failure)
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
                    Unrunnable::PermissionDenied if found == program => {
                        format!("{program}: permission denied")
                    }
                    Unrunnable::PermissionDenied => {
                        format!("{program}: permission denied ({found} is not executable)")
                    }
                    Unrunnable::Directory => names("a directory, not a command"),
                    Unrunnable::NotExecutable => names("a file, not an executable"),
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
            _ if self.is_path_as_text() => {
                Some(crate::process::path_as_text_hint(&self.program, None).into())
            }
            _ => None,
        }
    }

    /// `program` is a path to a directory or plain file written where a
    /// command goes, so it was most likely meant as text.
    pub(crate) fn is_path_as_text(&self) -> bool {
        matches!(
            &self.failure,
            Failure::NotRunnable(NotRunnable {
                found,
                why: Unrunnable::Directory | Unrunnable::NotExecutable,
            }) if *found == self.program
        )
    }
}

impl Failure {
    /// `program` stands for the path found when the spawn names none.
    pub(crate) fn from_spawn(program: &str, failure: crate::process::SpawnFailure) -> Self {
        use crate::process::{SpawnFailure as S, Unrunnable as U};
        let (found, why) = match failure {
            S::NotFound => return Self::NotFound,
            S::PermissionDenied { found } => (found, Unrunnable::PermissionDenied),
            S::NotAProgram { found, kind } => (
                found,
                match kind {
                    U::Directory => Unrunnable::Directory,
                    U::File => Unrunnable::NotExecutable,
                },
            ),
            S::Io(message) => (None, Unrunnable::Io(message.into())),
        };
        Self::NotRunnable(NotRunnable {
            found: found.map_or_else(|| program.to_owned(), crate::path::shown),
            why,
        })
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
