//! The schedules family of the desk.

use crate::enquiry::Add;
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::Datum;
use ral_core::types::Error;

use super::{DeskAct, ExarchDesk};

impl ExarchDesk {
    /// The self-wakeup guard the whole schedule family runs first. It is
    /// handed the tag the model typed, so the refusal names that and not a
    /// vocabulary the model was never taught.
    pub(super) fn require_schedule_grant(&self, verb: &str) -> Result<(), Error> {
        if self.services.fleet.launch.allow_schedule {
            return Ok(());
        }
        Err(Error::new(format!(
            "`{verb}` refused: this agent does not hold the self-wakeup grant. An agent \
                 that can wake itself indefinitely holds real authority, so the grant is off \
                 by default; relaunch with `--allow-schedule` if this session genuinely needs \
                 to schedule its own wakeups."
        )))
    }

    /// `` `add `` — arm a self-wakeup through
    /// [`ScheduleRegistry::schedule`](crate::schedule::ScheduleRegistry::schedule).
    /// The answer is the table it now appears in; its `next-s` column already
    /// says everything a receipt could.
    pub(super) fn schedule(
        &self,
        Add {
            trigger,
            label,
            prompt,
        }: Add,
    ) -> Result<FOValue, Error> {
        self.require_schedule_grant("exarch-schedules `add")?;
        let s = &self.services;

        // The row goes up after the registry call, so a refusal tiers instead
        // of reading as one that landed.
        let described = trigger.describe();
        let result = s
            .agent
            .schedules
            .schedule(trigger, prompt, label.clone(), &s.agent.mailbox);
        let (payload, content) = match &result {
            Ok(receipt) => (
                described,
                format!(
                    "scheduled '{}' ({}s to first fire)",
                    receipt.label,
                    receipt.next_in.as_secs()
                ),
            ),
            Err(e) => (format!("refused: {e}"), format!("could not schedule: {e}")),
        };
        s.commit_act(DeskAct::Schedule, Some(&label), payload, result.is_err());
        s.record_forensic(crate::record::Forensic::HarnessResult {
            text: content.clone(),
        });
        match result {
            Ok(_) => Ok(self.schedule_table()),
            Err(_) => Err(Error::new(content)),
        }
    }

    /// A snapshot of this agent's live wakeups — every `` `exarch-schedules ``
    /// tag's answer.
    pub(super) fn schedule_table(&self) -> FOValue {
        self.services.agent.schedules.list().encode()
    }

    /// `` `remove `` — take one scheduled wakeup off the table by label. Only
    /// the grant refusal raises; a label that was never there is a successful
    /// call answering a table that does not carry it.
    pub(super) fn unschedule(&self, label: &str) -> Result<FOValue, Error> {
        self.require_schedule_grant("exarch-schedules `remove")?;
        let s = &self.services;
        // The rail's payload column spells out the miss the table only implies,
        // since the verb has no argument of its own to show there.
        let removed = s.agent.schedules.unschedule(label);
        let (payload, content) = if removed {
            (String::new(), format!("unscheduled '{label}'"))
        } else {
            (
                "no live schedule by that label".to_string(),
                format!("no live schedule labelled '{label}'"),
            )
        };
        s.commit_act(DeskAct::Unschedule, Some(label), payload, !removed);
        s.record_forensic(crate::record::Forensic::HarnessResult { text: content });
        Ok(self.schedule_table())
    }
}
