//! The context family of the desk.

use crate::enquiry::{Evict, Note, Survey};
use crate::record::model::ContextSurvey;
use crate::record::{AgentLog, EditAuthority};
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::Datum;
use ral_core::types::Error;

use super::{DeskAct, ExarchDesk};

/// The rail subject a cut mints from a turn address, as runs: `"turns 41–43,
/// 50"`. Sorted here, since the model's own list need not be.
fn turns_subject(turns: &[u64]) -> String {
    let mut sorted = turns.to_vec();
    sorted.sort_unstable();
    format!("turns {}", crate::record::model::runs(&sorted))
}

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

    /// `` `exarch-context `evict `` — the turns the address names leave the
    /// context at once, wherever they lie, but for a user turn whose answers
    /// still have a resident turn outside the set; the model's optional
    /// `note` stands in the marker left where they were. Commits the act
    /// under `refused` on either outcome, and mirrors the surviving weight or
    /// the refusal onto the trace before answering the survey.
    pub(super) fn context_evict(&self, Evict { turns, note }: Evict) -> Result<FOValue, Error> {
        let note = note.map(|Note(note)| note);
        let payload = note
            .as_ref()
            .map_or_else(String::new, |note| format!("note {note}"));
        let subject = turns_subject(&turns);
        let evicted = self
            .services
            .log
            .borrow_mut()
            .evict(&turns, note, EditAuthority::Model);
        // The act row is this eviction's only row.
        self.services.commit_act(
            DeskAct::ContextEvict,
            Some(&subject),
            payload,
            evicted.is_err(),
        );
        let survey = evicted.map(|_| self.context_survey());
        let text = match &survey {
            Ok(survey) => format!("context is now {} serialized bytes", survey.total_bytes),
            Err(error) => error.clone(),
        };
        self.services
            .record_forensic(crate::record::Forensic::HarnessResult { text });
        survey
            .map(|survey| Survey::from(survey).encode())
            .map_err(Error::new)
    }
}
