//! Minting the record `try` hands its handler (`fact::ErrorRecord`) from an
//! [`Error`] and its [`Status`].

use super::{Error, Status};
use crate::fact::{ErrorRecord, Reason};
use crate::process::{CommandFailure, SpawnFailure, WorkerStop};
use crate::source::CallSite;
use crate::types::{Shell, Value};

impl From<&Status> for Reason {
    fn from(status: &Status) -> Self {
        match status {
            Status::Raised(_) => Self::Raised,
            Status::Process(CommandFailure::ExitCode(code)) => Self::Exited(*code),
            Status::Process(CommandFailure::Signal(sig)) => Self::Signaled(sig.number()),
            Status::Process(CommandFailure::Spawn(SpawnFailure::NotFound)) => Self::NotFound,
            Status::Process(CommandFailure::Spawn(_)) => Self::NotRunnable,
            Status::WorkerStopped(WorkerStop::Cancelled) => Self::Cancelled,
            Status::WorkerStopped(WorkerStop::TimedOut) => Self::TimedOut,
        }
    }
}

impl ErrorRecord {
    /// `status` is `status`'s own code and `reason` its projection, so the two
    /// cannot disagree.
    pub fn new(cmd: &str, status: &Status, message: &str, site: Option<CallSite>) -> Self {
        Self {
            cmd: cmd.into(),
            status: status.code(),
            reason: status.into(),
            message: message.into(),
            site,
        }
    }
}

/// `` `ok v | `err record ``: how `poll`'s settled arm and the report
/// envelope read a body's end.
pub fn outcome_value(outcome: Result<Value, ErrorRecord>) -> Value {
    match outcome {
        Ok(v) => Value::variant("ok", Some(v)),
        Err(record) => Value::variant("err", Some(Value::from_datum(record))),
    }
}

impl Error {
    /// This error as the record `try` hands its handler.  The failing command
    /// is the one the innermost dispatch stamped (`name_failure` in `types::flow`),
    /// `<runtime>` for a failure no dispatch owns; its
    /// position is the error's own span, or the run's call site when that
    /// lies outside the session's sources.
    pub fn record(&self, shell: &Shell) -> ErrorRecord {
        let own = self.span.and_then(|span| shell.session.sources.site(span));
        self.record_at(own.or_else(|| shell.call_site()))
    }

    /// [`Self::record`] at a position the caller already knows, `None` for
    /// one that has none.
    pub fn record_at(&self, site: Option<CallSite>) -> ErrorRecord {
        let cmd = self.command.as_deref().unwrap_or("<runtime>");
        let site = self.site.as_deref().cloned().or(site);
        ErrorRecord::new(cmd, &self.status, &self.message, site)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::FOValue;
    use crate::data::datum::{Datum, tag};

    #[test]
    fn status_is_the_reasons_own_projection() {
        let rec = ErrorRecord::new(
            "sh",
            &Status::Process(CommandFailure::ExitCode(3)),
            "x",
            None,
        );
        assert_eq!((rec.status(), rec.reason()), (3, Reason::Exited(3)));
        let cause = ErrorRecord::new(
            "sh",
            &Status::WorkerStopped(WorkerStop::TimedOut),
            "x",
            None,
        );
        assert_eq!((cause.status(), cause.reason()), (124, Reason::TimedOut));
    }

    #[test]
    fn a_wire_record_with_an_unknown_reason_is_refused() {
        let mut wire = ErrorRecord::new("sh", &Status::Raised(1), "x", None).encode();
        let FOValue::Map { entries } = &mut wire else {
            unreachable!()
        };
        let reason = entries.iter_mut().find(|(k, _)| k == "reason").unwrap();
        reason.1 = tag("teleported", None);
        assert!(ErrorRecord::decode(&wire).is_err());
    }
}
