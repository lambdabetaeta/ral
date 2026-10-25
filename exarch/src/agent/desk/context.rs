//! The context family of the desk.

use crate::enquiry::{Evict, Note, Survey};
use crate::record::model::ContextSurvey;
use crate::record::{AgentLog, EditAuthority};
use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::types::Error;

use super::{DeskAct, ExarchDesk};

impl ExarchDesk {
    /// The context as the log holds it. Silent, like the roster: a survey
    /// commits no act, and every edit tag answers one of these too.
    pub(super) fn context_survey(&self) -> ContextSurvey {
        self.services.log.borrow().context().context_survey()
    }

    /// The tail every `` `exarch-transcript `` tag shares: `locate` under the
    /// session lock, `read` once it is gone.  The family draws no row: a read
    /// is a listing, and a listing's telling is the record it answers with.
    pub(super) fn locate_then_read<P>(
        &self,
        locate: impl FnOnce(&mut AgentLog) -> Result<P, String>,
        read: impl FnOnce(P) -> Result<FOValue, String>,
    ) -> Result<FOValue, Error> {
        // Its own statement: the guard dies at this semicolon, so a long read
        // never holds the seam, the bus and `/resources` behind it.
        let located = locate(&mut self.services.log.borrow_mut());
        located.and_then(read).map_err(Error::new)
    }

    /// `` `exarch-context `evict `` — the run `[from, to]` leaves the
    /// context at once, one residue standing for it with the model's note.
    /// Commits the act under `refused` on either outcome, and mirrors the
    /// surviving weight or the refusal onto the trace before answering the
    /// survey.
    pub(super) fn context_evict(
        &self,
        Evict {
            span,
            note: Note(note),
        }: Evict,
    ) -> Result<FOValue, Error> {
        let payload = format!("note {note}");
        let evicted = self
            .services
            .log
            .borrow_mut()
            .evict(span, Some(note), EditAuthority::Model);
        // The act row is this eviction's only row.
        self.services.commit_act(
            DeskAct::ContextEvict,
            Some(&span.to_string()),
            payload,
            evicted.is_err(),
        );
        let survey = evicted.map(|()| self.context_survey());
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
        self.services
            .log
            .borrow()
            .record_emitter()
            .emit_or_report(crate::record::Forensic::HarnessResult { text });
        survey
            .map(|survey| Survey::from(survey).encode())
            .map_err(Error::new)
    }
}
