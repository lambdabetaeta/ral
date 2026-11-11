//! Minting the record `try` hands its handler.

use crate::error::Error;
use crate::fact::{ErrorFact, tag};
use crate::shell::runtime::{Shell, Value};

/// `` `ok v | `err record ``: how `poll`'s settled arm and the report
/// envelope read a body's end.
pub fn outcome_value(outcome: Result<Value, ErrorFact>) -> Value {
    match outcome {
        Ok(v) => Value::variant(tag::OK, Some(v)),
        Err(fact) => Value::variant(tag::ERR, Some(Value::failure(fact))),
    }
}

impl Error {
    /// This failure, its provenance resolved against `shell`.
    pub fn fact(&self, shell: &Shell) -> ErrorFact {
        let prov = self.prov_or(shell.local.audit.call_site());
        ErrorFact::new(
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
    use crate::fact::Reason;
    use crate::source::CallSite;

    fn site() -> CallSite {
        CallSite {
            script: "test.ral".into(),
            line: 1,
            col: 1,
        }
    }

    #[test]
    fn a_wire_record_with_an_unknown_reason_is_refused() {
        let mut wire = ErrorFact::new(Reason::raised("x"), "sh".into(), site()).encode();
        let FOValue::Map { entries } = &mut wire else {
            unreachable!()
        };
        let reason = entries.iter_mut().find(|(k, _)| k == "reason").unwrap();
        reason.1 = tag("teleported", None);
        assert!(ErrorFact::decode(&wire).is_err());
    }
}
