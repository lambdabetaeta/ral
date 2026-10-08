//! Cancellation-aware retry policy and idle budgets for the attempt loops in
//! `stream.rs`, keyed on the variant `error.rs` classified.

use super::tls::STREAM_IDLE_TIMEOUT;
use super::{CancelSite, ProviderError, Refusal};
use crate::cancel;
use crate::clock;
use jiff::Timestamp;
use std::time::Duration;

/// Retry budget for transient stream and network failures.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
/// A 429 asks for time; it is not a broken request, so it earns a longer leash.
const RATE_LIMIT_MAX_ATTEMPTS: u32 = 6;
const BASE_DELAY_MS: u64 = 750;
const MAX_DELAY_MS: u64 = 8_000;
/// High enough to honour the tens of seconds a `Retry-After` commonly asks for.
const RATE_LIMIT_MAX_DELAY_MS: u64 = 30_000;
const RETRY_IDLE_TIMEOUT: Duration = Duration::from_mins(1);

/// A server-named wait the patient tier sits out in place: only
/// [`Wait::patient`] makes one, so no provider value can stall the loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wait(Duration);

impl Wait {
    /// `None` past the patient tier's ceiling: a wait the loop does not sit
    /// out in place.
    pub(super) fn patient(wait: Duration) -> Option<Self> {
        (wait <= Duration::from_millis(RATE_LIMIT_MAX_DELAY_MS)).then_some(Self(wait))
    }

    pub fn get(self) -> Duration {
        self.0
    }
}

/// What the retry loop does with a refusal, read from its named wait alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// Back off and ask again, for the server's own wait when it named one.
    InPlace(Option<Wait>),
    /// The reset lies past what the loop waits in place: surfaced at once.
    Deferred(Timestamp),
}

impl Refusal {
    /// The one reading of its wait, shared by the loop, the renderers, the
    /// hold and the resume.
    pub fn recovery(&self) -> Recovery {
        let Some(at) = self.resets_at else {
            return Recovery::InPlace(None);
        };
        match Wait::patient(clock::until(at, self.received)) {
            Some(wait) => Recovery::InPlace(Some(wait)),
            None => Recovery::Deferred(at),
        }
    }
}

/// The first attempt keeps the full [`STREAM_IDLE_TIMEOUT`]; retries take the
/// shorter one, spending the attempt budget rather than the clock.
pub(super) fn idle_timeout(attempt: u32) -> Duration {
    if attempt <= 1 {
        STREAM_IDLE_TIMEOUT
    } else {
        RETRY_IDLE_TIMEOUT
    }
}

/// The server's own [`Wait`] wins when present; the shift cap only guards
/// `1u64 << shift` against overflow.
async fn backoff_sleep(attempt: u32, retry_after: Option<Wait>, max_delay_ms: u64) {
    if let Some(wait) = retry_after {
        tokio::time::sleep(wait.0).await;
        return;
    }
    let shift = attempt.saturating_sub(1).min(16);
    let delay_ms = BASE_DELAY_MS
        .saturating_mul(1u64 << shift)
        .min(max_delay_ms);
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
}

pub(super) enum Attempt<T> {
    /// A completed attempt, including a committed partial stream.
    Done(T),
    Failed(ProviderError),
}

/// Drive `one` through the shared retry policy: only `Transient` and a
/// `Refused` waited in place are retried, each against its own budget, and
/// cancellation races the backoff sleep, so a cancel mid-delay never waits it
/// out.
pub(super) async fn retry_with_backoff<T>(
    cancel_site: CancelSite,
    cancel: &cancel::Token,
    mut one: impl AsyncFnMut(u32) -> Attempt<T>,
) -> Result<T, ProviderError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled(cancel_site));
        }
        let mut error = match one(attempt).await {
            Attempt::Done(value) => return Ok(value),
            Attempt::Failed(error @ ProviderError::Cancelled(_)) => return Err(error),
            Attempt::Failed(error) => error,
        };
        let (max_attempts, max_delay_ms, retry_after) = match &error {
            ProviderError::Refused(refusal) => match refusal.recovery() {
                Recovery::InPlace(wait) => (RATE_LIMIT_MAX_ATTEMPTS, RATE_LIMIT_MAX_DELAY_MS, wait),
                Recovery::Deferred(_) => return Err(error),
            },
            ProviderError::Transient { .. } => (MAX_ATTEMPTS, MAX_DELAY_MS, None),
            _ => return Err(error),
        };
        if attempt >= max_attempts {
            if let ProviderError::Transient { attempts, .. } = &mut error {
                *attempts = attempt;
            }
            return Err(error);
        }
        tokio::select! {
            biased;
            () = wait_for_cancel(cancel) => {
                return Err(ProviderError::Cancelled(cancel_site));
            }
            () = backoff_sleep(attempt, retry_after, max_delay_ms) => {}
        }
    }
}

/// [`cancel::Token`] is a bare atomic with no waker, so a cancel is noticed
/// only on the next poll — the 50ms is that latency.
pub(super) async fn wait_for_cancel(cancel: &cancel::Token) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Limit;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build retry test runtime")
    }

    #[test]
    fn idle_timeout_before_token_is_retried_then_surfaced() {
        let calls = std::cell::Cell::new(0u32);
        let out: Result<(), ProviderError> = runtime().block_on(retry_with_backoff(
            CancelSite::BeforeRequest,
            &cancel::Token::new(),
            async |_attempt| {
                calls.set(calls.get() + 1);
                Attempt::Failed(ProviderError::Transient {
                    cause: "stream idle: no response within timeout".into(),
                    attempts: 1,
                    body: None,
                    status: None,
                })
            },
        ));
        assert_eq!(calls.get(), MAX_ATTEMPTS);
        assert!(matches!(
            out,
            Err(ProviderError::Transient {
                attempts: MAX_ATTEMPTS,
                ..
            })
        ));
    }

    /// A refusal naming a distant reset is not retried.
    #[test]
    fn a_429_naming_a_distant_reset_surfaces_on_its_first_attempt() {
        let error = ProviderError::Refused(Refusal::for_test(
            Limit::Rate,
            Some(Duration::from_mins(10)),
        ));
        let calls = std::cell::Cell::new(0u32);
        let out: Result<(), ProviderError> = runtime().block_on(retry_with_backoff(
            CancelSite::BeforeRequest,
            &cancel::Token::new(),
            async |_attempt| {
                calls.set(calls.get() + 1);
                Attempt::Failed(error.clone())
            },
        ));
        assert_eq!(calls.get(), 1);
        assert!(matches!(out, Err(ProviderError::Refused(_))));
    }

    #[test]
    fn recovery_waits_in_place_up_to_the_patient_ceiling() {
        let after =
            |secs: Option<u64>| Refusal::for_test(Limit::Rate, secs.map(Duration::from_secs));
        assert_eq!(
            after(Some(30)).recovery(),
            Recovery::InPlace(Wait::patient(Duration::from_secs(30)))
        );
        let late = after(Some(31));
        assert_eq!(late.recovery(), Recovery::Deferred(late.resets_at.unwrap()));
        assert_eq!(after(None).recovery(), Recovery::InPlace(None));
    }

    #[test]
    fn idle_timeout_budget_stays_bounded() {
        let worst_case: Duration = (1..=MAX_ATTEMPTS).map(idle_timeout).sum();
        assert!(worst_case < Duration::from_mins(10));
        assert_eq!(idle_timeout(1), STREAM_IDLE_TIMEOUT);
        assert!(idle_timeout(2) < STREAM_IDLE_TIMEOUT);
    }

    #[test]
    fn cancellation_interrupts_backoff() {
        runtime().block_on(async {
            let cancel = cancel::Token::new();
            let cancel_during_wait = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                cancel_during_wait.cancel(ral_core::process::CancelCause::Interrupted);
            });
            let out: Result<(), ProviderError> =
                retry_with_backoff(CancelSite::Backoff, &cancel, async |_| {
                    Attempt::Failed(ProviderError::Transient {
                        cause: "retry me".into(),
                        attempts: 1,
                        body: None,
                        status: None,
                    })
                })
                .await;
            assert!(matches!(
                out,
                Err(ProviderError::Cancelled(CancelSite::Backoff))
            ));
        });
    }
}
