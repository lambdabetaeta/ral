//! What a provider discloses about how much of an account's entitlement is
//! left: a rationed window (five hours, seven days) or a credit purse.
//!
//! Plain data — no provider trace, no network, no rendering beyond the one
//! row each allowance draws.

mod meters;

use crate::agent::resources::hms;
use crate::bus::card::{Card, Field, FieldVal, Mark, Readout, Span};
use crate::provider::credential::Roster;
use crate::provider::identity::{AccountId, Meter};
use crate::provider::listing::Fetches;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::time::Duration;

/// One rationed window of an account's entitlement, as the provider reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct Allowance {
    /// The span this allowance renews over: five hours, seven days. `None` for
    /// a balance that renews by payment rather than by clock — a credit purse.
    pub window: Option<Duration>,
    pub used: Consumption,
    /// Unix seconds, absolute. A provider reporting a *relative* reset is
    /// converted at parse, not at render: a reading may sit on a channel
    /// between the two, and a relative figure would quietly age.
    pub resets_at: Option<u64>,
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
    /// The proportion consumed, `None` when the provider disclosed no bound —
    /// then there is nothing to draw a bar against.
    pub fn fraction(&self) -> Option<f64> {
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

    /// This allowance as one aligned row.
    pub fn field(&self) -> Field {
        self.field_at(crate::bootstrap::now_secs())
    }

    /// [`field`](Self::field) with `now` supplied rather than read from the
    /// clock, so the label's reset clause is exactly testable.
    fn field_at(&self, now: u64) -> Field {
        let window_part = match self.window {
            Some(d) => window_words(d),
            None => "balance".to_string(),
        };
        let reset_part = self
            .resets_at
            .filter(|&r| r > now)
            .map(|r| format!("resets in {}", hms(r - now, " ")));
        let label = match reset_part {
            Some(rp) => format!("{window_part} · {rp}"),
            None => window_part,
        };
        let value = if let Some(f) = self.fraction() {
            FieldVal::Readout(Readout {
                value: percent_from_fraction(f),
                max: Some(100),
                unit: Some("%".into()),
            })
        } else {
            let Consumption::Counted { used, limit, unit } = &self.used else {
                unreachable!("fraction() is None only for Consumption::Counted")
            };
            FieldVal::Inline(vec![Span::plain(inline_text(*used, *limit, *unit))])
        };
        Field { label, value }
    }
}

/// The proportion as a whole percentage, `0..=100`. A nonzero fraction never
/// rounds to `0`: a bar reading empty when the ration has started is a lie
/// the user acts on.
fn percent_from_fraction(f: f64) -> u32 {
    let clamped = f.clamp(0.0, 1.0);
    let rounded = (clamped * 100.0).round();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0.0..=100.0 above"
    )]
    let percent = rounded as u32;
    if clamped > 0.0 && percent == 0 {
        1
    } else {
        percent
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

fn plural(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit}")
    } else {
        format!("{n} {unit}s")
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

/// One section per account — its label as the heading — over its rows.
///
/// Unmetered accounts draw no section; when every account is one, the card
/// names them in a sentence rather than reading as a claim of no limit.
pub fn limits_card(readings: &[(String, Reading)]) -> Card {
    if readings
        .iter()
        .all(|(_, r)| matches!(r, Reading::Unmetered))
    {
        let names = readings
            .iter()
            .map(|(label, _)| label.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let sentence = if readings.is_empty() {
            "there are no accounts to check for a ration".to_string()
        } else {
            format!(
                "none of {names} publishes a ration — /limits reports subscriptions and credit balances"
            )
        };
        return Card(vec![Mark::Text {
            spans: vec![Span::plain(sentence)],
        }]);
    }
    let mut marks = Vec::new();
    for (label, reading) in readings {
        let mark = match reading {
            Reading::Allowances(allowances) => {
                let mut sorted: Vec<&Allowance> = allowances.iter().collect();
                sorted.sort_by(|a, b| match (a.window, b.window) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    (None, None) => Ordering::Equal,
                });
                Mark::Fields {
                    rows: sorted.iter().map(|a| a.field()).collect(),
                }
            }
            Reading::Unmetered => continue,
            Reading::Failed(reason) => Mark::Text {
                spans: vec![Span::plain(format!("{label}: {reason}"))],
            },
        };
        marks.push(Mark::heading(label));
        marks.push(mark);
    }
    Card(marks)
}

/// What an account's service will say about its own ration. The one seam the
/// network sits behind, so a survey is testable without it.
pub trait MeterSource {
    /// `Ok(vec![])` is a genuine answer — an account whose service meters
    /// nothing.
    ///
    /// # Errors
    /// A refusal is a sentence naming the account by its label.
    fn read(&self, account: &AccountId) -> Result<Vec<Allowance>, String>;
}

/// The live source: each account's meter, read over the roster's credentials.
/// `Clone` is cheap, so a survey hands a copy to each background thread.
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
    fn read(&self, id: &AccountId) -> Result<Vec<Allowance>, String> {
        let account = self
            .roster
            .account(id)
            .ok_or_else(|| format!("{id} is not a known account"))?;
        let Some(meter) = account.service.meter else {
            return Ok(Vec::new());
        };
        let credential = self
            .roster
            .credential(id)
            .ok_or_else(|| format!("{} has no resolved credential", self.roster.label(account)))?;
        match meter {
            Meter::Codex => meters::codex::read(account, credential, &self.roster),
            Meter::OpenRouterCredits => meters::openrouter::read(account, credential, &self.roster),
        }
    }
}

/// Every available account's ration, fetched concurrently.
///
/// There is no cache and no store: a reading is an instantaneous fact about a
/// rolling window, and a TTL would hand the user a stale number in exactly the
/// situation where they typed `/limits` because they suspected one.
pub struct Survey {
    fetches: Fetches<AccountId, Vec<Allowance>>,
    /// Computed at [`Self::open`], on the caller's thread, from the full
    /// account set — [`identity::label`] needs the set, and the set must not
    /// cross to a background thread piecemeal.
    labels: Vec<(AccountId, String)>,
}

impl Survey {
    pub fn open<S: MeterSource + Clone + Send + 'static>(roster: &Roster, source: &S) -> Self {
        let labels = roster
            .accounts()
            .iter()
            .map(|account| (account.id.clone(), roster.label(account)))
            .collect();
        let fetches = Fetches::new();
        for account in roster.accounts() {
            let id = account.id.clone();
            let source = source.clone();
            fetches.spawn(id.clone(), move || source.read(&id));
        }
        Self { fetches, labels }
    }

    /// Block until every fetch has reported, then compose the card. Called on
    /// the survey thread, never the UI thread.
    pub fn settle(self) -> Card {
        let mut results: BTreeMap<AccountId, Result<Vec<Allowance>, String>> =
            self.fetches.settle().into_iter().collect();
        // Sorted back into the roster's order — the fetches' arrival order is
        // not a fact anyone should read anything into.
        let readings: Vec<(String, Reading)> = self
            .labels
            .into_iter()
            .map(|(id, label)| {
                let reading = match results.remove(&id) {
                    Some(Ok(allowances)) if allowances.is_empty() => Reading::Unmetered,
                    Some(Ok(allowances)) => Reading::Allowances(allowances),
                    Some(Err(reason)) => Reading::Failed(reason),
                    None => Reading::Failed("no reading arrived".to_string()),
                };
                (label, reading)
            })
            .collect();
        limits_card(&readings)
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
    fn field_percent_never_rounds_a_nonzero_fraction_to_zero() {
        let barely_started = Allowance {
            window: None,
            used: Consumption::Fraction(0.004),
            resets_at: None,
        };
        let FieldVal::Readout(r) = barely_started.field_at(0).value else {
            panic!("a disclosed fraction renders as a readout");
        };
        assert_eq!(r.value, 1);

        let untouched = Allowance {
            window: None,
            used: Consumption::Fraction(0.0),
            resets_at: None,
        };
        let FieldVal::Readout(r) = untouched.field_at(0).value else {
            panic!("a disclosed fraction renders as a readout");
        };
        assert_eq!(r.value, 0);
    }

    #[test]
    fn field_label_states_window_and_reset() {
        let now = 1_000_000;
        let five_hours = Allowance {
            window: Some(Duration::from_hours(5)),
            used: Consumption::Fraction(0.1),
            resets_at: Some(now + 3600 + 120),
        };
        let label = five_hours.field_at(now).label;
        assert!(label.contains("5 hours"));
        assert!(label.contains("resets in"));

        let no_window = Allowance {
            window: None,
            used: Consumption::Fraction(0.1),
            resets_at: None,
        };
        assert_eq!(no_window.field_at(now).label, "balance");

        let already_past = Allowance {
            window: Some(Duration::from_hours(1)),
            used: Consumption::Fraction(0.1),
            resets_at: Some(now - 10),
        };
        let label = already_past.field_at(now).label;
        assert!(
            !label.contains("resets in"),
            "a past reset yields no clause"
        );
        assert!(label.contains("1 hour"));
    }

    #[test]
    fn field_value_renders_raw_figures_with_and_without_a_cap() {
        let capless = counted(1240, None, Unit::Dollars);
        let FieldVal::Inline(spans) = capless.field_at(0).value else {
            panic!("an undisclosed cap renders inline");
        };
        assert_eq!(spans[0].text, "$12.40 used · no cap");

        let capped = counted(30, Some(100), Unit::Tokens);
        let FieldVal::Readout(_) = capped.field_at(0).value else {
            panic!("a disclosed cap yields a fraction, hence a readout");
        };
    }

    #[test]
    fn sort_orders_five_hour_before_weekly() {
        let weekly = Allowance {
            window: Some(Duration::from_hours(7 * 24)),
            used: Consumption::Fraction(0.2),
            resets_at: None,
        };
        let five_hour = Allowance {
            window: Some(Duration::from_hours(5)),
            used: Consumption::Fraction(0.5),
            resets_at: None,
        };
        let card = limits_card(&[(
            "anthropic".to_string(),
            Reading::Allowances(vec![weekly, five_hour]),
        )]);
        let Mark::Fields { rows } = &card.0[1] else {
            panic!("an allowances reading renders one fields mark");
        };
        assert!(rows[0].label.contains("5 hours"));
        assert!(rows[1].label.contains("7 days"));
    }

    #[test]
    fn all_unmetered_collapses_to_one_sentence_naming_the_accounts() {
        let card = limits_card(&[
            ("anthropic".to_string(), Reading::Unmetered),
            ("openai".to_string(), Reading::Unmetered),
            ("deepseek".to_string(), Reading::Unmetered),
        ]);
        assert_eq!(card.0.len(), 1);
        let Mark::Text { spans } = &card.0[0] else {
            panic!("the collapsed card is one text mark");
        };
        assert!(spans[0].text.contains("anthropic, openai, deepseek"));
        assert!(spans[0].text.contains("publishes a ration"));
    }

    #[test]
    fn empty_readings_say_something_sensible() {
        let card = limits_card(&[]);
        assert_eq!(card.0.len(), 1);
    }

    fn declared_account(name: &str) -> crate::provider::identity::Account {
        use crate::provider::identity::{Auth, Billing, Service, ServiceName};
        crate::provider::identity::Account::of_service(Service {
            name: ServiceName::declared(name).unwrap(),
            endpoint: Some(format!("https://{name}.example/v1/")),
            adapter: genai::adapter::AdapterKind::OpenAI,
            default_model: None,
            auth: Auth::Env(format!("{}_KEY", name.to_uppercase())),
            billing: Billing::Metered,
            routes: false,
            meter: None,
        })
    }

    fn roster_of(accounts: &[crate::provider::identity::Account]) -> Roster {
        let mut roster = Roster::default();
        for account in accounts {
            roster.admit(
                account.clone(),
                crate::provider::credential::Credential::ApiKey("test-key".into()),
            );
        }
        roster
    }

    /// Follows `listing.rs`'s own `FakeSource` pattern, so a survey is
    /// testable with no network at all.
    #[derive(Clone)]
    struct FakeSource {
        readings: std::sync::Arc<BTreeMap<AccountId, Result<Vec<Allowance>, String>>>,
    }

    impl MeterSource for FakeSource {
        fn read(&self, account: &AccountId) -> Result<Vec<Allowance>, String> {
            self.readings
                .get(account)
                .cloned()
                .unwrap_or_else(|| Err("no fake reading".into()))
        }
    }

    #[test]
    fn a_survey_draws_only_the_accounts_with_something_to_report() {
        let loaded = declared_account("loaded");
        let unmetered = declared_account("unmetered");
        let failing = declared_account("failing");
        let roster = roster_of(&[loaded.clone(), unmetered.clone(), failing.clone()]);

        let allowance = Allowance {
            window: None,
            used: Consumption::Fraction(0.5),
            resets_at: None,
        };
        let mut readings = BTreeMap::new();
        readings.insert(loaded.id, Ok(vec![allowance.clone()]));
        readings.insert(unmetered.id.clone(), Ok(Vec::new()));
        readings.insert(failing.id.clone(), Err("network is down".to_string()));
        let source = FakeSource {
            readings: std::sync::Arc::new(readings),
        };

        let card = Survey::open(&roster, &source).settle();

        assert_eq!(
            card.0.len(),
            4,
            "a section mark plus a body mark for each account with something to \
             report — the unmetered one draws nothing at all"
        );
        let Mark::Fields { rows } = &card.0[1] else {
            panic!("the loaded account renders its allowances");
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, allowance.field().label);

        let Mark::Text { spans } = &card.0[3] else {
            panic!("the failing account renders its failure sentence");
        };
        assert!(spans[0].text.contains("network is down"));
        assert!(
            spans[0].text.contains(&roster.label(&failing)),
            "the failure sentence names the account by its label: {}",
            spans[0].text
        );

        let named = card
            .0
            .iter()
            .filter_map(|mark| match mark {
                Mark::Text { spans } => Some(spans[0].text.clone()),
                _ => None,
            })
            .collect::<String>();
        assert!(
            !named.contains(&roster.label(&unmetered)),
            "an account with no ration to report is not mentioned: {named}"
        );
    }
}
