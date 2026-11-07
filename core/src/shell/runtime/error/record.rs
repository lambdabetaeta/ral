//! Minting the record `try` hands its handler.

use crate::error::Error;
use crate::fact::{ErrorRecord, tag};
use crate::shell::runtime::{Shell, Value};

/// `` `ok v | `err record ``: how `poll`'s settled arm and the report
/// envelope read a body's end.
pub fn outcome_value(outcome: Result<Value, ErrorRecord>) -> Value {
    match outcome {
        Ok(v) => Value::variant(tag::OK, Some(v)),
        Err(record) => Value::variant(tag::ERR, Some(Value::from_datum(record))),
    }
}

impl Error {
    pub fn record(&self, shell: &Shell) -> ErrorRecord {
        let prov = self.prov_or(shell.local.audit.call_site());
        ErrorRecord::new(
            self.reason.clone(),
            prov.name().into(),
            prov.site(&shell.session.sources),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::FOValue;
    use crate::data::datum::{Datum, tag};
    use crate::fact::{Failure, FailureCode as _, Reason};
    use crate::source::CallSite;

    fn site() -> CallSite {
        CallSite {
            script: "test.ral".into(),
            line: 1,
            col: 1,
        }
    }

    #[test]
    fn reason_is_the_failure_codes_only_source() {
        let rec = ErrorRecord::new(
            Reason::process("sh", Failure::Exited(3)),
            "sh".into(),
            site(),
        );
        assert_eq!(rec.failure_code().get(), 3);
        let cause = ErrorRecord::new(Reason::TimedOut, "sh".into(), site());
        assert_eq!(cause.failure_code().get(), 124);
    }

    #[test]
    fn a_wire_record_with_an_unknown_reason_is_refused() {
        let mut wire = ErrorRecord::new(Reason::Raised("x".into()), "sh".into(), site()).encode();
        let FOValue::Map { entries } = &mut wire else {
            unreachable!()
        };
        let reason = entries.iter_mut().find(|(k, _)| k == "reason").unwrap();
        reason.1 = tag("teleported", None);
        assert!(ErrorRecord::decode(&wire).is_err());
    }
}
