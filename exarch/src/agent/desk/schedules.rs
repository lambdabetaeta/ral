//! The schedules family of the desk.

use crate::bus::Txn;
use crate::enquiry::Add;
use crate::schedule::{ScheduleInfo, Trigger};
use jiff::Timestamp;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::error::Error;
use ral_core::fact::{
    ExarchFailure, ScheduleFailure, ScheduleFailureNoGrant, ScheduleFailureRecording,
};
use ral_core::shell::runtime::no_desk;

use super::{DeskAct, ExarchDesk};

impl ExarchDesk {
    /// The self-wakeup guard the whole schedule family runs first. It is
    /// handed the tag the model typed, so the refusal names that and not a
    /// vocabulary the model was never taught.
    pub(super) fn require_schedule_grant(&self, verb: &str) -> Result<(), Error> {
        if self.call.fleet.launch.allow_schedule {
            return Ok(());
        }
        Err(failure(ScheduleFailure::NoGrant(ScheduleFailureNoGrant {
            verb: verb.into(),
        })))
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
        let mut body = self.loan.get().ok_or_else(no_desk)?;
        let call = &self.call;
        let trigger = Trigger::from(trigger);

        // The row goes up after the registry call, so a refusal tiers instead
        // of reading as one that landed.
        let described = trigger.describe();
        let result = call
            .handle
            .0
            .address
            .transact(|txn| {
                body.schedules
                    .schedule(txn, trigger, prompt, label.clone())
                    .map(|receipt| (receipt, live_table(txn)))
            })
            .map_err(|why| recording(Some(&label), Some(&described), &why))
            .and_then(|scheduled| scheduled.map_err(failure));
        let (payload, content) = match &result {
            Ok((receipt, _)) => (
                described,
                format!(
                    "scheduled '{}' ({}s to first fire)",
                    receipt.label,
                    receipt.next_in.as_secs()
                ),
            ),
            Err(e) => (
                format!("refused: {}", e.plain()),
                format!("could not schedule: {}", e.plain()),
            ),
        };
        call.commit_act(
            &body,
            DeskAct::Schedule,
            Some(&label),
            payload,
            result.is_err(),
        );
        body.log
            .record_emitter()
            .emit_or_end(crate::record::Forensic::HarnessResult { text: content });
        result.map(|(_, table)| table)
    }

    /// A snapshot of this agent's live wakeups — every `` `exarch-schedules ``
    /// tag's answer.
    pub(super) fn schedule_table(&self) -> Result<FOValue, Error> {
        let _body = self.loan.get().ok_or_else(no_desk)?;
        self.call
            .handle
            .0
            .address
            .transact(live_table)
            .map_err(|why| recording(None, None, &why))
    }

    /// `` `remove `` — take one scheduled wakeup off the table by label. Only
    /// a grant refusal or recording failure raises; an absent label is a successful
    /// call answering a table that does not carry it.
    pub(super) fn unschedule(&self, label: &str) -> Result<FOValue, Error> {
        self.require_schedule_grant("exarch-schedules `remove")?;
        let mut body = self.loan.get().ok_or_else(no_desk)?;
        let call = &self.call;
        // The rail's payload column spells out the miss the table only implies,
        // since the verb has no argument of its own to show there.
        let (removed, table) = call
            .handle
            .0
            .address
            .try_transact(|txn| {
                let removed = body.schedules.unschedule(txn, label)?;
                Ok((removed, live_table(txn)))
            })
            .map_err(|why| recording(Some(label), None, &why))?;
        let (payload, content) = if removed {
            (String::new(), format!("unscheduled '{label}'"))
        } else {
            (
                "no live schedule by that label".to_string(),
                format!("no live schedule labelled '{label}'"),
            )
        };
        call.commit_act(&body, DeskAct::Unschedule, Some(label), payload, !removed);
        body.log
            .record_emitter()
            .emit_or_end(crate::record::Forensic::HarnessResult { text: content });
        Ok(table)
    }
}

fn live_table(txn: &mut Txn<'_>) -> FOValue {
    let now = Timestamp::now();
    txn.schedules()
        .iter()
        .map(|(label, schedule)| ScheduleInfo::of(label, schedule, now))
        .collect::<Vec<_>>()
        .encode()
}

fn failure(cause: ScheduleFailure) -> Error {
    super::failure(ExarchFailure::Schedule(cause))
}

fn recording(
    label: Option<&str>,
    trigger: Option<&str>,
    why: &crate::record::Unavailable,
) -> Error {
    failure(ScheduleFailure::Recording(ScheduleFailureRecording {
        label: label.map(str::to_owned),
        trigger: trigger.map(str::to_owned),
        cause: why.failure(),
    }))
}
