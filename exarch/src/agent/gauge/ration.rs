//! The ration gauge, in two audiences. The user hears [`USER_HEARS`] once per
//! account, on latches [`Rations`](crate::provider::Rations) keeps; the model
//! is reminded at [`MODEL_HEARS`] on latches of the agent's own.

use super::Warning;
use crate::agent::nudge::Reminder;
use crate::latch::Latch;
use crate::provider::allowance::Allowance;
use crate::provider::{Account, AccountId};
use std::collections::HashMap;
use std::time::Duration;

/// Percent spent at which the user is told, once per account.
pub(crate) const USER_HEARS: &[u32] = &[50, 75, 90, 95];
/// Percent spent at which each agent's model is reminded.
const MODEL_HEARS: &[u32] = &[90, 95];

/// The model's reminders: every agent keeps its own latches, since each model
/// needs its own.
#[derive(Default)]
pub(crate) struct Ration {
    latches: HashMap<(AccountId, Option<Duration>), Latch>,
}

impl Ration {
    /// The model's reminders for windows newly reaching [`MODEL_HEARS`];
    /// `resumes` is whether this agent is resumed at a reset.
    pub(crate) fn climb(
        &mut self,
        account: &Account,
        allowances: &[Allowance],
        resumes: bool,
    ) -> Vec<Warning> {
        allowances
            .iter()
            .filter_map(|a| {
                let pct = a.percent()?;
                self.latches
                    .entry((account.id.clone(), a.window))
                    .or_default()
                    .climb(MODEL_HEARS, pct)?;
                Some(Warning::Model(Reminder::Ration {
                    service: account.service.name.as_str().to_string(),
                    label: a.label(),
                    pct,
                    windowed: a.window.is_some(),
                    resumes,
                }))
            })
            .collect()
    }
}

/// The user's lines for the windows [`Rations::climb`](crate::provider::Rations)
/// reported newly reached.
pub(crate) fn told(account: &Account, climbed: &[Allowance]) -> Vec<Warning> {
    let service = account.service.name.as_str();
    climbed
        .iter()
        .filter_map(|a| {
            let (pct, label) = (a.percent()?, a.label());
            Some(Warning::User(format!(
                "usage: {service} at {pct}%; {label}"
            )))
        })
        .collect()
}

#[cfg(test)]
mod tests;
