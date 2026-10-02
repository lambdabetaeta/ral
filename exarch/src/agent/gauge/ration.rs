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
                    label: a.field().label,
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
            let (pct, label) = (a.percent()?, a.field().label);
            Some(Warning::User(format!(
                "usage: {service} at {pct}% — {label}"
            )))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::allowance::Consumption;

    fn window(hours: u64, fraction: f64) -> Allowance {
        Allowance {
            window: Some(Duration::from_hours(hours)),
            used: Consumption::Fraction(fraction),
            resets_at: None,
        }
    }

    fn read(ration: &mut Ration, a: &[Allowance], resumes: bool) -> Vec<Warning> {
        ration.climb(&Account::built_in("openrouter"), a, resumes)
    }

    fn ration(warning: &Warning) -> (u32, bool, bool) {
        let Warning::Model(Reminder::Ration {
            pct,
            windowed,
            resumes,
            ..
        }) = warning
        else {
            panic!("the model is reminded");
        };
        (*pct, *windowed, *resumes)
    }

    #[test]
    fn crossing_ninety_reminds_the_model_once_with_the_outcome_by_resumption() {
        let mut ration_latches = Ration::default();
        assert!(read(&mut ration_latches, &[window(5, 0.80)], true).is_empty());
        let warnings = read(&mut ration_latches, &[window(5, 0.91)], true);
        let [warning] = warnings.as_slice() else {
            panic!("one reminder at 90");
        };
        assert_eq!(ration(warning), (91, true, true));
        assert!(read(&mut ration_latches, &[window(5, 0.92)], true).is_empty());

        let warnings = read(&mut Ration::default(), &[window(5, 0.91)], false);
        assert_eq!(ration(&warnings[0]), (91, true, false));
    }

    #[test]
    fn windows_latch_apart() {
        let mut ration_latches = Ration::default();
        assert_eq!(
            read(
                &mut ration_latches,
                &[window(5, 0.91), window(168, 0.91)],
                true
            )
            .len(),
            2
        );
        assert!(
            read(
                &mut ration_latches,
                &[window(5, 0.91), window(168, 0.91)],
                true
            )
            .is_empty()
        );
        assert_eq!(
            read(
                &mut ration_latches,
                &[window(5, 0.91), window(168, 0.96)],
                true
            )
            .len(),
            1,
            "only the weekly window climbed"
        );
    }

    #[test]
    fn a_purse_is_told_apart_from_a_window() {
        let purse = Allowance {
            window: None,
            used: Consumption::Fraction(0.96),
            resets_at: None,
        };
        let warnings = read(&mut Ration::default(), &[purse], true);
        assert_eq!(ration(&warnings[0]), (96, false, true));
    }

    #[test]
    fn an_allowance_with_no_fraction_is_skipped() {
        let uncapped = Allowance {
            window: None,
            used: Consumption::Counted {
                used: 5,
                limit: None,
                unit: crate::provider::allowance::Unit::Requests,
            },
            resets_at: None,
        };
        assert!(
            read(
                &mut Ration::default(),
                std::slice::from_ref(&uncapped),
                true
            )
            .is_empty()
        );
        assert!(told(&Account::built_in("openrouter"), &[uncapped]).is_empty());
    }

    #[test]
    fn told_formats_the_user_line() {
        let warnings = told(&Account::built_in("openrouter"), &[window(5, 0.80)]);
        let [Warning::User(line)] = warnings.as_slice() else {
            panic!("one line for the user");
        };
        assert!(
            line.starts_with("usage: openrouter at 80% — 5 hours"),
            "{line}"
        );
    }
}
