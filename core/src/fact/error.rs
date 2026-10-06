//! The record `try` hands its handler, `poll` its `` `err `` payload, and the
//! report envelope its failed outcome.  Bytes are absent by design: `audit` is
//! the forensic path.  Minted from an `Error` in `types::error`.

use crate::process::CancelCause;
use crate::source::CallSite;
use crate::{record, variant};

/// Why a failure happened, as the tagged value `$err[reason]` reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Exited(i32),
    Signaled(i32),
    Cancelled(CancelCause),
    NotFound,
    NotRunnable,
    Raised,
}

variant!(typed Reason {
    Exited(i32): "exited",
    Signaled(i32): "signaled",
    Cancelled(CancelCause): "cancelled",
    NotFound: "not-found",
    NotRunnable: "not-runnable",
    Raised: "raised",
});

/// A failure as ral code sees it: the failing command, its status, why, and
/// where.  `status` is the projection of the `Status` that gave `reason`, and
/// only `ErrorRecord::new` makes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorRecord {
    pub(crate) cmd: String,
    pub(crate) status: i32,
    pub(crate) reason: Reason,
    pub(crate) message: String,
    pub(crate) site: Option<CallSite>,
}

record!(typed ErrorRecord {
    cmd: "cmd",
    status: "status",
    reason: "reason",
    message: "message",
    site: "site",
});

impl ErrorRecord {
    pub fn cmd(&self) -> &str {
        &self.cmd
    }

    pub fn status(&self) -> i32 {
        self.status
    }

    pub fn reason(&self) -> Reason {
        self.reason
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn site(&self) -> Option<&CallSite> {
        self.site.as_ref()
    }
}
