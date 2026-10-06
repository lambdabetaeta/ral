//! What is known of each account's allowance, shared by every agent and
//! frontend in the process: the last reading of its meter, and the instants
//! refusals said it is held until — the whole account for a spent allowance,
//! one model for a rate. Fed on the one road every request
//! travels, `Provider::complete`; read by the ration gauge and by `/limits`.

use super::allowance::{Allowance, MeterSource};
use super::retry::Recovery;
use super::{Account, AccountId, Limit, Meter, ProviderError, Refusal, StepOut};
use crate::latch::Latch;
use ral_core::sync::LockExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_mins(2);

#[derive(Default)]
pub struct Rations {
    accounts: Mutex<HashMap<AccountId, Standing>>,
}

#[derive(Default)]
struct Standing {
    /// The last reading, whole; empty until one lands.
    reading: Vec<Allowance>,
    /// When the last read was spawned.
    asked: Option<Instant>,
    /// Until when the plan's allowance is spent: every model is held.
    allowance_until: Option<jiff::Timestamp>,
    /// Until when each model's rate is spent: that model alone is held.
    rate_until: HashMap<String, jiff::Timestamp>,
    /// The user's latches, one per window: a rung is told once per account,
    /// by whichever agent climbs it first.
    told: HashMap<Option<Duration>, Latch>,
}

impl Standing {
    /// The refusal a request for `model` meets unsent: the later hold on it,
    /// when that lies past what the retry loop waits in place. A nearer one is
    /// let through, for the provider's own refusal to be waited out there.
    fn hold(&self, account: &Account, model: &str, now: jiff::Timestamp) -> Option<Refusal> {
        let (limit, until) = [
            (Limit::Allowance, self.allowance_until),
            (Limit::Rate, self.rate_until.get(model).copied()),
        ]
        .into_iter()
        .filter_map(|(limit, until)| Some((limit, until?)))
        .max_by_key(|&(_, until)| until)?;
        let service = &account.service.name;
        let cause = match limit {
            Limit::Allowance => {
                format!("nothing was sent: {service}'s plan allowance is spent until it resets")
            }
            Limit::Rate => {
                format!(
                    "nothing was sent: {model} on {service} is held until its rate limit resets"
                )
            }
        };
        let refusal = Refusal {
            limit,
            resets_at: Some(until),
            received: now,
            cause,
            body: None,
        };
        matches!(refusal.recovery(), Recovery::Deferred(_)).then_some(refusal)
    }
}

impl Rations {
    /// The gate before a request: refused by a [`Standing::hold`], nothing
    /// sent — else a read of the account's meter spawned when one is due, its
    /// reading landing in the record when it does.
    pub(crate) fn admit<S: MeterSource>(
        self: &Arc<Self>,
        account: &Account,
        model: &str,
        source: &S,
    ) -> Result<(), ProviderError> {
        let due = self.with(&account.id, |standing| {
            if let Some(refusal) = standing.hold(account, model, jiff::Timestamp::now()) {
                return Err(ProviderError::Refused(refusal));
            }
            let due = !matches!(account.service.meter, Meter::Unpublished)
                && standing.asked.is_none_or(|at| at.elapsed() >= INTERVAL);
            if due {
                standing.asked = Some(Instant::now());
            }
            Ok(due)
        })?;
        if due {
            let (rations, source, account) = (Arc::clone(self), source.clone(), account.clone());
            std::thread::spawn(move || {
                if let Ok(reading) = source.read(&account) {
                    rations.land(&account.id, reading);
                }
            });
        }
        Ok(())
    }

    /// After a request: a refusal's reset is kept, the later of old and new,
    /// on the scope of what it says ran out.
    pub(crate) fn settle(
        &self,
        account: &AccountId,
        model: &str,
        outcome: &Result<StepOut, ProviderError>,
    ) {
        if let Err(ProviderError::Refused(Refusal {
            limit,
            resets_at: Some(at),
            ..
        })) = outcome
        {
            self.with(account, |standing| match limit {
                Limit::Allowance => {
                    standing.allowance_until = standing.allowance_until.max(Some(*at));
                }
                Limit::Rate => {
                    let held = standing.rate_until.entry(model.to_string()).or_insert(*at);
                    *held = (*held).max(*at);
                }
            });
        }
    }

    /// The last reading of `account`; empty until one lands.
    pub fn reading(&self, account: &AccountId) -> Vec<Allowance> {
        self.accounts
            .lock_ignore_poison()
            .get(account)
            .map(|standing| standing.reading.clone())
            .unwrap_or_default()
    }

    /// The windows of `account`'s last reading that newly reach a rung of
    /// `ladder`, climbed on latches every agent on the account shares.
    pub(crate) fn climb(&self, account: &AccountId, ladder: &[u32]) -> Vec<Allowance> {
        self.with(account, |standing| {
            let Standing { reading, told, .. } = standing;
            reading
                .iter()
                .filter(|a| {
                    a.percent()
                        .and_then(|pct| told.entry(a.window).or_default().climb(ladder, pct))
                        .is_some()
                })
                .cloned()
                .collect()
        })
    }

    /// A reading that landed, from a spawned read or a survey.
    pub(super) fn land(&self, account: &AccountId, reading: Vec<Allowance>) {
        self.with(account, |standing| standing.reading = reading);
    }

    /// `f` on `account`'s standing, the map locked for exactly that call.
    fn with<R>(&self, account: &AccountId, f: impl FnOnce(&mut Standing) -> R) -> R {
        f(self
            .accounts
            .lock_ignore_poison()
            .entry(account.clone())
            .or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::allowance::Consumption;
    use ral_core::test_helper::eventually;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn reading(fraction: f64) -> Vec<Allowance> {
        vec![Allowance {
            window: None,
            used: Consumption::Fraction(fraction),
            resets_at: None,
        }]
    }

    #[derive(Clone)]
    struct FakeSource {
        answer: Result<Vec<Allowance>, String>,
        calls: Arc<AtomicUsize>,
    }

    impl FakeSource {
        fn answering(answer: Result<Vec<Allowance>, String>) -> Self {
            Self {
                answer,
                calls: Arc::default(),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl MeterSource for FakeSource {
        fn read(&self, _: &Account) -> Result<Vec<Allowance>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.answer.clone()
        }
    }

    fn refused(limit: Limit, resets_at: jiff::Timestamp) -> Result<StepOut, ProviderError> {
        Err(ProviderError::Refused(Refusal {
            resets_at: Some(resets_at),
            ..Refusal::for_test(limit, None)
        }))
    }

    fn rate_until(rations: &Rations, account: &Account, model: &str) -> Option<jiff::Timestamp> {
        rations.with(&account.id, |standing| {
            standing.rate_until.get(model).copied()
        })
    }

    fn allowance_until(rations: &Rations, account: &Account) -> Option<jiff::Timestamp> {
        rations.with(&account.id, |standing| standing.allowance_until)
    }

    /// An allowance holds every model, a rate its own alone; a hold the loop
    /// would wait out in place, or one already past, lets the request through.
    #[test]
    fn a_hold_refuses_what_ran_out_while_its_reset_is_deferred() {
        let account = Account::built_in("anthropic");
        let source = FakeSource::answering(Ok(Vec::new()));
        let admit = |limit, resets_at, model: &str| {
            let rations = Arc::new(Rations::default());
            rations.settle(&account.id, "a", &refused(limit, resets_at));
            rations.admit(&account, model, &source)
        };
        let now = jiff::Timestamp::now();
        let hour = now + Duration::from_hours(1);
        let Err(ProviderError::Refused(refusal)) = admit(Limit::Allowance, hour, "b") else {
            panic!("an allowance holds every model");
        };
        assert!(
            refusal.cause.contains("nothing was sent"),
            "{}",
            refusal.cause
        );
        assert!(admit(Limit::Rate, hour, "a").is_err());
        assert!(admit(Limit::Rate, hour, "b").is_ok());
        assert!(admit(Limit::Allowance, now + Duration::from_secs(10), "a").is_ok());
        assert!(admit(Limit::Allowance, now - Duration::from_secs(1), "a").is_ok());
    }

    #[test]
    fn settle_keeps_the_later_of_two_instants_per_scope() {
        let rations = Rations::default();
        let account = Account::built_in("anthropic");
        let now = jiff::Timestamp::now();
        let (soon, late) = (now + Duration::from_mins(1), now + Duration::from_hours(1));
        let later = late + Duration::from_mins(1);
        for limit in [Limit::Allowance, Limit::Rate] {
            rations.settle(&account.id, "a", &refused(limit, late));
            rations.settle(&account.id, "a", &refused(limit, soon));
        }
        assert_eq!(allowance_until(&rations, &account), Some(late));
        assert_eq!(rate_until(&rations, &account, "a"), Some(late));
        rations.settle(&account.id, "a", &refused(Limit::Rate, later));
        assert_eq!(rate_until(&rations, &account, "a"), Some(later));
        assert_eq!(allowance_until(&rations, &account), Some(late));
        assert_eq!(rate_until(&rations, &account, "b"), None);
    }

    #[test]
    fn a_rung_is_climbed_once_per_account() {
        let rations = Rations::default();
        let account = Account::built_in("openrouter");
        rations.land(&account.id, reading(0.80));
        let ladder = [50, 75, 90, 95];
        assert_eq!(rations.climb(&account.id, &ladder), reading(0.80));
        assert_eq!(rations.climb(&account.id, &ladder), Vec::<Allowance>::new());
    }

    #[test]
    fn an_unpublished_account_is_never_read() {
        let rations = Arc::new(Rations::default());
        let account = Account::built_in("anthropic");
        let source = FakeSource::answering(Ok(reading(0.5)));
        rations.admit(&account, "m", &source).unwrap();
        assert!(rations.with(&account.id, |standing| standing.asked.is_none()));
        assert_eq!(source.calls(), 0);
    }

    #[test]
    fn a_published_account_is_read_once_per_interval() {
        let rations = Arc::new(Rations::default());
        let account = Account::built_in("openrouter");
        let source = FakeSource::answering(Ok(reading(0.5)));
        rations.admit(&account, "m", &source).unwrap();
        // A read lands on a thread of its own.
        eventually(Duration::from_secs(5), || {
            (!rations.reading(&account.id).is_empty()).then_some(())
        })
        .expect("the read landed");
        assert_eq!(rations.reading(&account.id), reading(0.5));
        rations.admit(&account, "m", &source).unwrap();
        assert_eq!(source.calls(), 1, "the second admit is inside INTERVAL");
    }

    #[test]
    fn a_failed_read_leaves_the_last_reading_standing() {
        let rations = Arc::new(Rations::default());
        let account = Account::built_in("openrouter");
        rations.land(&account.id, reading(0.5));
        let source = FakeSource::answering(Err("network is down".into()));
        rations.admit(&account, "m", &source).unwrap();
        eventually(Duration::from_secs(5), || {
            (source.calls() == 1).then_some(())
        })
        .expect("the read was asked for");
        assert_eq!(source.calls(), 1);
        assert_eq!(rations.reading(&account.id), reading(0.5));
    }
}
