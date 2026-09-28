//! What is known of each account's allowance, shared by every agent and
//! frontend in the process: the last reading of its meter, and the instant a
//! refusal said it is spent until. Fed on the one road every request
//! travels, `Provider::complete`; read by the ration gauge and by `/limits`.

use super::allowance::{Allowance, MeterSource};
use super::{Account, AccountId, Meter, ProviderError, StepOut};
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
    /// From a refusal: no request is sent before this instant.
    spent_until: Option<jiff::Timestamp>,
}

impl Rations {
    /// The gate before a request: refused while a refusal's reset is still
    /// ahead — nothing is sent — else a read of the account's meter spawned
    /// when one is due, its reading landing in the record when it does.
    pub(crate) fn admit<S>(
        self: &Arc<Self>,
        account: &Account,
        source: &S,
    ) -> Result<(), ProviderError>
    where
        S: MeterSource + Clone + Send + 'static,
    {
        let due = self.with(&account.id, |standing| {
            if let Some(until) = standing.spent_until
                && until > jiff::Timestamp::now()
            {
                return Err(ProviderError::Exhausted {
                    resets_at: until,
                    cause: format!(
                        "nothing was sent: {}'s allowance is spent until it resets",
                        account.service.name
                    ),
                    body: None,
                });
            }
            let due = !matches!(account.service.meter, Meter::Unpublished)
                && standing.asked.is_none_or(|at| at.elapsed() >= INTERVAL);
            if due {
                standing.asked = Some(Instant::now());
            }
            Ok(due)
        })?;
        if due {
            let (rations, source, id) = (Arc::clone(self), source.clone(), account.id.clone());
            std::thread::spawn(move || {
                if let Ok(reading) = source.read(&id) {
                    rations.land(&id, reading);
                }
            });
        }
        Ok(())
    }

    /// After a request: a refusal's reset is kept, the later of old and new.
    pub(crate) fn settle(&self, account: &AccountId, outcome: &Result<StepOut, ProviderError>) {
        if let Err(ProviderError::Exhausted { resets_at, .. }) = outcome {
            self.with(account, |standing| {
                standing.spent_until = standing.spent_until.max(Some(*resets_at));
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
    use crate::provider::{ServiceName, built_in};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn account(name: &str) -> Account {
        Account::of_service(built_in(&ServiceName::declared(name).unwrap()).unwrap())
    }

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
        fn read(&self, _: &AccountId) -> Result<Vec<Allowance>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.answer.clone()
        }
    }

    fn exhausted(resets_at: jiff::Timestamp) -> Result<StepOut, ProviderError> {
        Err(ProviderError::Exhausted {
            resets_at,
            cause: "429".into(),
            body: None,
        })
    }

    fn spent_until(rations: &Rations, account: &Account) -> Option<jiff::Timestamp> {
        rations.with(&account.id, |standing| standing.spent_until)
    }

    /// A read lands on a thread of its own.
    fn eventually(done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_refusal_holds_the_account_until_its_reset_passes() {
        let rations = Arc::new(Rations::default());
        let account = account("anthropic");
        let source = FakeSource::answering(Ok(Vec::new()));
        let until = jiff::Timestamp::now() + Duration::from_hours(1);
        rations.settle(&account.id, &exhausted(until));

        let Err(ProviderError::Exhausted {
            resets_at, cause, ..
        }) = rations.admit(&account, &source)
        else {
            panic!("a request inside the hold is refused");
        };
        assert_eq!(resets_at, until);
        assert!(cause.contains("nothing was sent"), "{cause}");

        rations.with(&account.id, |standing| {
            standing.spent_until = Some(jiff::Timestamp::now() - Duration::from_secs(1));
        });
        assert!(rations.admit(&account, &source).is_ok());
    }

    #[test]
    fn settle_keeps_the_later_of_two_instants() {
        let rations = Rations::default();
        let account = account("anthropic");
        let now = jiff::Timestamp::now();
        let (soon, late) = (now + Duration::from_mins(1), now + Duration::from_hours(1));
        rations.settle(&account.id, &exhausted(late));
        rations.settle(&account.id, &exhausted(soon));
        assert_eq!(spent_until(&rations, &account), Some(late));
        rations.settle(&account.id, &exhausted(late + Duration::from_mins(1)));
        assert_eq!(
            spent_until(&rations, &account),
            Some(late + Duration::from_mins(1))
        );
    }

    #[test]
    fn an_unpublished_account_is_never_read() {
        let rations = Arc::new(Rations::default());
        let account = account("anthropic");
        let source = FakeSource::answering(Ok(reading(0.5)));
        rations.admit(&account, &source).unwrap();
        assert!(rations.with(&account.id, |standing| standing.asked.is_none()));
        assert_eq!(source.calls(), 0);
    }

    #[test]
    fn a_published_account_is_read_once_per_interval() {
        let rations = Arc::new(Rations::default());
        let account = account("openrouter");
        let source = FakeSource::answering(Ok(reading(0.5)));
        rations.admit(&account, &source).unwrap();
        eventually(|| !rations.reading(&account.id).is_empty());
        assert_eq!(rations.reading(&account.id), reading(0.5));
        rations.admit(&account, &source).unwrap();
        assert_eq!(source.calls(), 1, "the second admit is inside INTERVAL");
    }

    #[test]
    fn a_failed_read_leaves_the_last_reading_standing() {
        let rations = Arc::new(Rations::default());
        let account = account("openrouter");
        rations.land(&account.id, reading(0.5));
        let source = FakeSource::answering(Err("network is down".into()));
        rations.admit(&account, &source).unwrap();
        eventually(|| source.calls() == 1);
        assert_eq!(source.calls(), 1);
        assert_eq!(rations.reading(&account.id), reading(0.5));
    }
}
