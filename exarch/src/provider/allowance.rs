//! What a provider discloses about how much of an account's entitlement is
//! left: a rationed window (five hours, seven days) or a credit purse.
//!
//! Plain data — no provider trace, no network, no rendering beyond the one
//! row each allowance draws.

mod meters;

use crate::clock::{self, hms};
use crate::provider::Rations;
use crate::provider::credential::Roster;
use crate::provider::identity::{Account, AccountId, Meter};
use crate::provider::listing::Fetches;
use jiff::Timestamp;
use ral_core::text::plural;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// One rationed window of an account's entitlement, as the provider reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Allowance {
    /// The span this allowance renews over: five hours, seven days. `None` for
    /// a balance that renews by payment rather than by clock — a credit purse.
    pub window: Option<Duration>,
    pub used: Consumption,
    /// Absolute. A provider reporting a *relative* reset is converted at
    /// parse, not at render: a reading may sit on a channel between the two,
    /// and a relative figure would quietly age.
    pub resets_at: Option<Timestamp>,
}

/// How much of an allowance is gone. Two arms because two disclosures exist:
/// some providers publish a proportion and keep the denominator to themselves.
#[derive(Clone, Debug, PartialEq)]
pub enum Consumption {
    /// A proportion, `0.0..=1.0`. The denominator is undisclosed, not one.
    Fraction(f64),
    /// Both sides, in the [`Unit`]'s own smallest indivisible amount.
    Counted {
        used: u64,
        limit: Option<u64>,
        unit: Unit,
    },
}

/// What a [`Consumption::Counted`] counts.
///
/// Money counts in cents, so a purse reporting $12.40 is `1240`: the scale is
/// a fact about the unit, which keeps the count exact and the figures the
/// provider's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Requests,
    Tokens,
    Dollars,
}

impl Allowance {
    /// The share consumed as the whole percentage the card shows, `None` when
    /// the provider disclosed no bound. A nonzero share never rounds to `0`: a
    /// bar reading empty when the ration has started is a lie the user acts on.
    pub fn percent(&self) -> Option<u32> {
        let share = self.fraction()?.clamp(0.0, 1.0);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "clamped to 0.0..=100.0"
        )]
        let percent = (share * 100.0).round() as u32;
        Some(if share > 0.0 { percent.max(1) } else { percent })
    }

    fn fraction(&self) -> Option<f64> {
        match self.used {
            Consumption::Fraction(f) => Some(f),
            Consumption::Counted {
                used,
                limit: Some(limit),
                ..
            } if limit > 0 => {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "a display proportion; a used/limit count this large has no exact f64 need"
                )]
                let ratio = used as f64 / limit as f64;
                Some(ratio)
            }
            Consumption::Counted { .. } => None,
        }
    }

    /// The window and, while it is still ahead, its reset, as the row's label.
    pub fn label_at(&self, now: Timestamp) -> String {
        let window_part = self
            .window
            .map_or_else(|| "balance".to_string(), window_words);
        let reset_part = self
            .resets_at
            .filter(|&r| r > now)
            .map(|r| format!("resets in {}", hms(clock::until(r, now).as_secs())));
        match reset_part {
            Some(rp) => format!("{window_part} · {rp}"),
            None => window_part,
        }
    }

    pub fn label(&self) -> String {
        self.label_at(Timestamp::now())
    }

    /// The figure as plain text: the counted form, or the percentage.
    pub fn figure_text(&self) -> String {
        match &self.used {
            Consumption::Counted { used, limit, unit } if self.percent().is_none() => {
                inline_text(*used, *limit, *unit)
            }
            _ => format!("{}%", self.percent().unwrap_or_default()),
        }
    }
}

/// `5 hours`, `7 days`, `30 minutes` — the largest whole unit that divides
/// the duration, singular at 1. Never a provider-supplied string.
fn window_words(d: Duration) -> String {
    let secs = d.as_secs();
    if secs.is_multiple_of(86400) {
        plural(secs / 86400, "day")
    } else if secs.is_multiple_of(3600) {
        plural(secs / 3600, "hour")
    } else if secs.is_multiple_of(60) {
        plural(secs / 60, "minute")
    } else {
        plural(secs, "second")
    }
}

/// The raw figures, for a `Consumption::Counted` whose proportion is
/// undisclosed (no cap) or degenerate (a cap of zero).
fn inline_text(used: u64, limit: Option<u64>, unit: Unit) -> String {
    let used_text = match unit {
        Unit::Dollars => format!("{} used", dollars(used)),
        Unit::Requests => format!("{used} requests"),
        Unit::Tokens => format!("{used} tokens"),
    };
    match limit {
        None => format!("{used_text} · no cap"),
        Some(limit) => {
            let limit_text = match unit {
                Unit::Dollars => dollars(limit),
                Unit::Requests | Unit::Tokens => limit.to_string(),
            };
            format!("{used_text} · cap {limit_text}")
        }
    }
}

/// Cents as the amount a human reads.
fn dollars(cents: u64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// One account's reading of its own service: the allowances it disclosed, an
/// explicit statement that the service publishes nothing to ration, or why
/// the reading failed.
pub enum Reading {
    Allowances(Vec<Allowance>),
    Unmetered,
    Failed(String),
}

/// What an account's service will say about its own ration. The one seam the
/// network sits behind, so a survey is testable without it. `Clone` is cheap:
/// a survey hands a copy to each background thread.
pub trait MeterSource: Clone + Send + 'static {
    /// `Ok(vec![])` is a genuine answer — an account whose service meters
    /// nothing.
    ///
    /// # Errors
    /// A refusal is a sentence naming the account by its label.
    fn read(&self, account: &Account) -> Result<Vec<Allowance>, String>;
}

/// The live source: each account's meter, read over the roster's credentials.
#[derive(Clone)]
pub struct LiveMeters {
    roster: Roster,
}

impl LiveMeters {
    pub fn new(roster: Roster) -> Self {
        Self { roster }
    }
}

impl MeterSource for LiveMeters {
    fn read(&self, account: &Account) -> Result<Vec<Allowance>, String> {
        let read = match account.service.meter {
            Meter::Unpublished => return Ok(Vec::new()),
            Meter::OpenRouterCredits => meters::openrouter::read,
        };
        let credential = self
            .roster
            .credential(&account.id)
            .ok_or_else(|| format!("{} has no resolved credential", self.roster.label(account)))?;
        read(account, credential, &self.roster)
    }
}

/// Every available account's ration, fetched concurrently.
///
/// There is no cache and no TTL for the card: a reading is an instantaneous
/// fact about a rolling window, and a TTL would hand the user a stale number
/// in exactly the situation where they typed `/limits` because they suspected
/// one. Each reading is landed in the [`Rations`] record as well, where it
/// goes on being useful.
pub struct Survey {
    fetches: Fetches<AccountId, Vec<Allowance>>,
    /// Computed at [`Self::open`], on the caller's thread, from the full
    /// account set — [`label`](crate::provider::identity::label) needs the set, and the set must not
    /// cross to a background thread piecemeal.
    labels: Vec<(AccountId, String)>,
    rations: Arc<Rations>,
}

impl Survey {
    pub fn open<S: MeterSource>(roster: &Roster, source: &S, rations: Arc<Rations>) -> Self {
        let labels = roster
            .accounts()
            .iter()
            .map(|account| (account.id.clone(), roster.label(account)))
            .collect();
        let fetches = Fetches::new();
        for account in roster.accounts() {
            let (source, account) = (source.clone(), account.clone());
            fetches.spawn(account.id.clone(), move || source.read(&account));
        }
        Self {
            fetches,
            labels,
            rations,
        }
    }

    /// Block until every fetch has reported, land each reading in the record,
    /// then hand back the readings in roster order. Called on the survey
    /// thread, never the UI thread.
    pub fn settle(self) -> Vec<(String, Reading)> {
        let mut results: BTreeMap<AccountId, Result<Vec<Allowance>, String>> =
            self.fetches.settle().into_iter().collect();
        // Sorted back into the roster's order — the fetches' arrival order is
        // not a fact anyone should read anything into.
        self.labels
            .into_iter()
            .map(|(id, label)| {
                let reading = match results.remove(&id) {
                    Some(Ok(allowances)) => {
                        self.rations.land(&id, allowances.clone());
                        if allowances.is_empty() {
                            Reading::Unmetered
                        } else {
                            Reading::Allowances(allowances)
                        }
                    }
                    Some(Err(reason)) => Reading::Failed(reason),
                    None => Reading::Failed("no reading arrived".to_string()),
                };
                (label, reading)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counted(used: u64, limit: Option<u64>, unit: Unit) -> Allowance {
        Allowance {
            window: Some(Duration::from_hours(5)),
            used: Consumption::Counted { used, limit, unit },
            resets_at: None,
        }
    }

    #[test]
    fn fraction_reads_both_consumption_arms() {
        let by_fraction = Allowance {
            window: None,
            used: Consumption::Fraction(0.42),
            resets_at: None,
        };
        assert_eq!(by_fraction.fraction(), Some(0.42));

        let capped = counted(30, Some(100), Unit::Requests);
        assert_eq!(capped.fraction(), Some(0.3));

        let uncapped = counted(30, None, Unit::Requests);
        assert_eq!(uncapped.fraction(), None);

        let zero_cap = counted(30, Some(0), Unit::Requests);
        assert_eq!(zero_cap.fraction(), None);
    }

    #[test]
    fn percent_never_rounds_a_nonzero_fraction_to_zero() {
        let barely_started = Allowance {
            window: None,
            used: Consumption::Fraction(0.004),
            resets_at: None,
        };
        assert_eq!(barely_started.percent(), Some(1));

        let untouched = Allowance {
            window: None,
            used: Consumption::Fraction(0.0),
            resets_at: None,
        };
        assert_eq!(untouched.percent(), Some(0));
    }

    #[test]
    fn label_states_window_and_reset() {
        let now = Timestamp::from_second(1_000_000).unwrap();
        let five_hours = Allowance {
            window: Some(Duration::from_hours(5)),
            used: Consumption::Fraction(0.1),
            resets_at: Some(now + Duration::from_mins(62)),
        };
        let label = five_hours.label_at(now);
        assert!(label.contains("5 hours"));
        assert!(label.contains("resets in"));

        let no_window = Allowance {
            window: None,
            used: Consumption::Fraction(0.1),
            resets_at: None,
        };
        assert_eq!(no_window.label_at(now), "balance");

        let already_past = Allowance {
            window: Some(Duration::from_hours(1)),
            used: Consumption::Fraction(0.1),
            resets_at: Some(now - Duration::from_secs(10)),
        };
        let label = already_past.label_at(now);
        assert!(
            !label.contains("resets in"),
            "a past reset yields no clause"
        );
        assert!(label.contains("1 hour"));
    }

    #[test]
    fn figure_text_renders_raw_figures_with_and_without_a_cap() {
        let capless = counted(1240, None, Unit::Dollars);
        assert_eq!(capless.figure_text(), "$12.40 used · no cap");

        let capped = counted(30, Some(100), Unit::Requests);
        assert_eq!(capped.figure_text(), "30%");
    }

    fn roster_of(accounts: &[Account]) -> Roster {
        let mut store = crate::provider::credential::CredentialStore::default();
        for account in accounts {
            store.declare(account.clone());
            store.save_key(&account.id, "test-key".into());
        }
        store.roster()
    }

    /// Follows `listing.rs`'s own `FakeSource` pattern, so a survey is
    /// testable with no network at all.
    #[derive(Clone)]
    struct FakeSource {
        readings: std::sync::Arc<BTreeMap<AccountId, Result<Vec<Allowance>, String>>>,
    }

    impl MeterSource for FakeSource {
        fn read(&self, account: &Account) -> Result<Vec<Allowance>, String> {
            self.readings
                .get(&account.id)
                .cloned()
                .unwrap_or_else(|| Err("no fake reading".into()))
        }
    }

    #[test]
    fn a_survey_draws_only_the_accounts_with_something_to_report() {
        let loaded = Account::declared("loaded");
        let unmetered = Account::declared("unmetered");
        let failing = Account::declared("failing");
        let roster = roster_of(&[loaded.clone(), unmetered.clone(), failing.clone()]);

        let allowance = Allowance {
            window: None,
            used: Consumption::Fraction(0.5),
            resets_at: None,
        };
        let mut readings = BTreeMap::new();
        readings.insert(loaded.id.clone(), Ok(vec![allowance.clone()]));
        readings.insert(unmetered.id.clone(), Ok(Vec::new()));
        readings.insert(failing.id.clone(), Err("network is down".to_string()));
        let source = FakeSource {
            readings: std::sync::Arc::new(readings),
        };

        let rations = Arc::new(Rations::default());
        let readings = Survey::open(&roster, &source, Arc::clone(&rations)).settle();
        assert_eq!(
            rations.reading(&loaded.id),
            vec![allowance.clone()],
            "a survey lands what it read in the record"
        );

        assert_eq!(
            readings.len(),
            3,
            "one reading per account, in the roster's order"
        );
        let (label, Reading::Allowances(rows)) = &readings[0] else {
            panic!("the loaded account reads its allowances");
        };
        assert_eq!(label, &roster.label(&loaded));
        assert_eq!(rows, &vec![allowance]);

        let (label, reading) = &readings[1];
        assert_eq!(label, &roster.label(&unmetered));
        assert!(matches!(reading, Reading::Unmetered));

        let (label, Reading::Failed(reason)) = &readings[2] else {
            panic!("the failing account reads its failure");
        };
        assert_eq!(label, &roster.label(&failing));
        assert_eq!(reason, "network is down");
    }
}
