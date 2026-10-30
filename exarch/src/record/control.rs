//! The head seeds the folds; every later control record steps them.

use super::{Control, Mail, Model, Record, Recorded, Refusal, Schedules, Started};

pub(super) fn seed(started: &Started) -> (Mail, Schedules, Model) {
    (
        Mail::new(started),
        Schedules::new(),
        Model::new(started.selected.clone()),
    )
}

/// Borrowed for one commit; only the cell and reopen hold this capability.
pub(crate) struct Folds<'a> {
    pub(crate) mail: &'a mut Mail,
    pub(crate) schedules: &'a mut Schedules,
    pub(crate) model: &'a mut Model,
}

impl Folds<'_> {
    pub(super) fn step(&mut self, record: &Recorded<Record>) -> Result<(), Refusal> {
        let result = match record.value() {
            Record::Control(Control::SessionStarted(_)) => {
                Err("SessionStarted is only the head of a log".into())
            }
            Record::Control(control) => {
                let at = Recorded::new(record.locus().clone(), control);
                self.mail
                    .step(&at)
                    .and_then(|()| self.schedules.step(&at))
                    .and_then(|()| self.model.step(&at))
            }
            Record::Transcript(_) | Record::Display(_) | Record::Forensic(_) => Ok(()),
        };
        result.map_err(|reason| Refusal::Foreign {
            record: Box::new(record.value().clone()),
            reason,
        })
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct Replay {
    mail: Mail,
    schedules: Schedules,
    model: Model,
}

impl Replay {
    pub(crate) fn open(head: &Recorded<Record>) -> Result<Self, Refusal> {
        let Record::Control(Control::SessionStarted(started)) = head.value() else {
            return Err(Refusal::Foreign {
                record: Box::new(head.value().clone()),
                reason: "the first record is not SessionStarted, so it is not a session's log"
                    .into(),
            });
        };
        let (mail, schedules, model) = seed(started);
        Ok(Self {
            mail,
            schedules,
            model,
        })
    }
    pub(crate) fn step(&mut self, record: &Recorded<Record>) -> Result<(), Refusal> {
        Folds {
            mail: &mut self.mail,
            schedules: &mut self.schedules,
            model: &mut self.model,
        }
        .step(record)
    }
    pub(crate) fn finish(self) -> (Mail, Schedules, Model) {
        (self.mail, self.schedules, self.model)
    }
}
