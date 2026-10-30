//! The context family of the desk.

use crate::enquiry::{Evict, Note, Survey};
use crate::record::context::ContextSurvey;
use crate::record::{AgentLog, EditAuthority};
use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::shell::runtime::Error;

use super::{DeskAct, ExarchDesk};

impl ExarchDesk {
    pub(super) fn context_survey(&self) -> Result<ContextSurvey, Error> {
        let body = self.loan.get().ok_or_else(super::no_desk)?;
        Ok(body.log.context().context_survey())
    }

    /// Locate against the loan, then release it before reading files.
    pub(super) fn locate_then_read<P>(
        &self,
        locate: impl FnOnce(&mut AgentLog) -> Result<P, String>,
        read: impl FnOnce(P) -> Result<FOValue, String>,
    ) -> Result<FOValue, Error> {
        let located = {
            let mut body = self.loan.get().ok_or_else(super::no_desk)?;
            locate(&mut body.log)
        };
        located.and_then(read).map_err(Error::new)
    }

    pub(super) fn context_evict(
        &self,
        Evict {
            span,
            note: Note(note),
        }: Evict,
    ) -> Result<FOValue, Error> {
        let mut body = self.loan.get().ok_or_else(super::no_desk)?;
        let payload = format!("note {note}");
        let evicted = body
            .log
            .evict(span, Some(note), EditAuthority::Model)
            .map_err(|why| why.to_string());
        self.call.commit_act(
            &body,
            DeskAct::ContextEvict,
            Some(&span.to_string()),
            payload,
            evicted.is_err(),
        );
        let survey = evicted.map(|()| body.log.context().context_survey());
        let text = match &survey {
            Ok(survey) => {
                let stand = if span.is_single() { "stands" } else { "stand" };
                format!(
                    "{span} now {stand} under your note; context is now {} serialized bytes",
                    survey.total_bytes
                )
            }
            Err(error) => error.clone(),
        };
        body.log
            .record_emitter()
            .emit_or_end(crate::record::Forensic::HarnessResult { text });
        survey
            .map(|survey| Survey::from(survey).encode())
            .map_err(Error::new)
    }
}
