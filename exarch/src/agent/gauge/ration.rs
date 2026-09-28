//! The ration gauge: each window of an account's last reading climbs
//! [`RATION`]'s rungs on a latch of its own.

use super::{Latch, Warning};
use crate::provider::allowance::Allowance;
use crate::provider::{Account, AccountId};
use std::collections::HashMap;
use std::time::Duration;

/// Percent spent. The user hears every rung; the model, those from
/// [`MODEL_HEARS`] up.
const RATION: &[u32] = &[50, 75, 90, 95];
const MODEL_HEARS: u32 = 90;

#[derive(Default)]
pub(crate) struct Ration {
    latches: HashMap<(AccountId, Option<Duration>), Latch>,
}

impl Ration {
    /// The rungs `allowances` newly reach on `account`. An allowance with no
    /// percentage has nothing to climb.
    pub(crate) fn climb(&mut self, account: &Account, allowances: &[Allowance]) -> Vec<Warning> {
        allowances
            .iter()
            .filter_map(|a| {
                let pct = a.percent()?;
                let latch = self
                    .latches
                    .entry((account.id.clone(), a.window))
                    .or_default();
                let rung = latch.climb(RATION, pct)?;
                Some(warning(rung, account, a, pct))
            })
            .collect()
    }
}

fn warning(rung: u32, account: &Account, allowance: &Allowance, pct: u32) -> Warning {
    let service = account.service.name.as_str();
    let label = allowance.field().label;
    Warning {
        user: Some(format!("usage: {service} at {pct}% — {label}")),
        model: (rung >= MODEL_HEARS).then(|| {
            let outcome = if allowance.window.is_some() {
                "If it runs out, this task pauses until it resets and then resumes."
            } else {
                "When it runs out, requests fail until it is topped up."
            };
            (
                format!("usage {pct}%"),
                format!("Your {service} usage allowance is {pct}% spent ({label}). {outcome}"),
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::allowance::Consumption;
    use crate::provider::{ServiceName, built_in};

    fn account() -> Account {
        Account::of_service(built_in(&ServiceName::declared("openrouter").unwrap()).unwrap())
    }

    fn window(hours: u64, fraction: f64) -> Allowance {
        Allowance {
            window: Some(Duration::from_hours(hours)),
            used: Consumption::Fraction(fraction),
            resets_at: None,
        }
    }

    fn read(ration: &mut Ration, a: &[Allowance]) -> Vec<Warning> {
        ration.climb(&account(), a)
    }

    #[test]
    fn a_first_reading_warns_once_at_the_highest_rung_passed_and_only_the_user() {
        let mut ration = Ration::default();
        let warnings = read(&mut ration, &[window(5, 0.80)]);
        let [warning] = warnings.as_slice() else {
            panic!("one warning at 75, got {}", warnings.len())
        };
        let line = warning.user.as_ref().expect("the user hears 75");
        assert!(
            line.starts_with("usage: openrouter at 80% — 5 hours"),
            "{line}"
        );
        assert!(warning.model.is_none(), "75 is not told to the model");
        assert!(read(&mut ration, &[window(5, 0.81)]).is_empty());
    }

    #[test]
    fn crossing_ninety_tells_the_model_too_and_a_higher_reading_is_silent() {
        let mut ration = Ration::default();
        read(&mut ration, &[window(5, 0.80)]);
        let warnings = read(&mut ration, &[window(5, 0.91)]);
        let [warning] = warnings.as_slice() else {
            panic!("one warning at 90, got {}", warnings.len())
        };
        assert!(warning.user.is_some());
        let (cause, body) = warning.model.as_ref().expect("the model hears 90");
        assert_eq!(cause, "usage 91%");
        assert!(
            body.contains("91% spent") && body.contains("pauses until it resets"),
            "{body}"
        );
        assert!(read(&mut ration, &[window(5, 0.93)]).is_empty());
    }

    #[test]
    fn a_fall_rearms_the_ladder() {
        let mut ration = Ration::default();
        read(&mut ration, &[window(5, 0.80)]);
        assert!(read(&mut ration, &[window(5, 0.10)]).is_empty());
        assert_eq!(read(&mut ration, &[window(5, 0.55)]).len(), 1, "50 again");
    }

    #[test]
    fn windows_latch_apart() {
        let mut ration = Ration::default();
        assert_eq!(
            read(&mut ration, &[window(5, 0.60), window(168, 0.60)]).len(),
            2
        );
        assert!(read(&mut ration, &[window(5, 0.60), window(168, 0.60)]).is_empty());
        assert_eq!(
            read(&mut ration, &[window(5, 0.60), window(168, 0.76)]).len(),
            1,
            "only the weekly window climbed"
        );
    }

    #[test]
    fn a_purse_tells_the_model_what_running_out_means() {
        let purse = Allowance {
            window: None,
            used: Consumption::Fraction(0.96),
            resets_at: None,
        };
        let warnings = read(&mut Ration::default(), &[purse]);
        let (_, body) = warnings[0].model.as_ref().expect("95 reaches the model");
        assert!(body.contains("topped up"), "{body}");
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
        assert!(read(&mut Ration::default(), &[uncapped]).is_empty());
    }
}
