//! The context family of the desk.

use crate::enquiry::{Evict, Note, Survey};
use crate::record::context::ContextSurvey;
use crate::record::{AgentLog, EditAuthority, Uncommitted};
use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::error::Error;
use ral_core::fact::{ContextFailure, ExarchFailure};
use ral_core::shell::runtime::no_desk;

use super::{DeskAct, ExarchDesk};

impl ExarchDesk {
    pub(super) fn context_survey(&self) -> Result<ContextSurvey, Error> {
        let body = self.loan.get().ok_or_else(no_desk)?;
        Ok(body.log.context().context_survey())
    }

    /// Locate against the loan, then release it before reading files.
    pub(super) fn locate_then_read<P>(
        &self,
        context: Option<&str>,
        locate: impl FnOnce(&mut AgentLog) -> Result<P, ContextFailure>,
        read: impl FnOnce(P) -> Result<FOValue, ContextFailure>,
    ) -> Result<FOValue, Error> {
        let located = {
            let mut body = self.loan.get().ok_or_else(no_desk)?;
            locate(&mut body.log)
        };
        located.and_then(read).map_err(|cause| {
            let error = super::failure(ExarchFailure::Context(cause));
            match context {
                Some(context) => error.with_context(context),
                None => error,
            }
        })
    }

    pub(super) fn context_evict(
        &self,
        Evict {
            span,
            note: Note(note),
        }: Evict,
    ) -> Result<FOValue, Error> {
        let mut body = self.loan.get().ok_or_else(no_desk)?;
        let payload = format!("note {note}");
        let evicted = body
            .log
            .evict(span, Some(note), EditAuthority::Model)
            .map_err(|why| match why {
                Uncommitted::Refused(cause) => super::failure(ExarchFailure::Context(cause))
                    .with_context("exarch-context `evict"),
                Uncommitted::Unavailable(why) => {
                    super::failure(ExarchFailure::Recording(why.failure()))
                }
            });
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
            Err(error) => error.plain(),
        };
        body.log
            .record_emitter()
            .emit_or_end(crate::record::Forensic::HarnessResult { text });
        survey.map(|survey| Survey::from(survey).encode())
    }
}
