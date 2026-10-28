//! Cancellation-aware retry policy for the attempt loop in `stream.rs`, keyed
//! on the variant `error.rs` classified.

use super::{CancelSite, ProviderError, Refusal};
use crate::clock;
use jiff::Timestamp;
use ral_core::process::Scope;
use std::time::Duration;

/// Retry budget for transient stream and network failures.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
/// A 429 asks for time; it is not a broken request, so it earns a longer leash.
const RATE_LIMIT_MAX_ATTEMPTS: u32 = 6;
const BASE_DELAY_MS: u64 = 750;
const MAX_DELAY_MS: u64 = 8_000;
/// High enough to honour the tens of seconds a `Retry-After` commonly asks for.
const RATE_LIMIT_MAX_DELAY_MS: u64 = 30_000;

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

/// `(max attempts, max delay ms, server wait)` the loop grants a failure;
/// `None` surfaces it at once.
fn budget(error: &ProviderError) -> Option<(u32, u64, Option<Wait>)> {
    match error {
        ProviderError::Refused(refusal) => match refusal.recovery() {
            Recovery::InPlace(wait) => {
                Some((RATE_LIMIT_MAX_ATTEMPTS, RATE_LIMIT_MAX_DELAY_MS, wait))
            }
            Recovery::Deferred(_) => None,
        },
        ProviderError::Transient { .. } => Some((MAX_ATTEMPTS, MAX_DELAY_MS, None)),
        _ => None,
    }
}

impl ProviderError {
    /// The retry driver's own verdict, shared with the stream's partial-reply rule.
    pub(super) fn retryable(&self) -> bool {
        budget(self).is_some()
    }
}

/// Drive `one` through the shared retry policy: only `Transient` and a
/// `Refused` waited in place are retried, each against its own budget, and
/// cancellation races the backoff sleep, so a cancel mid-delay never waits it
/// out. `one` sees `None` on the first attempt, then the failure it replaces.
pub(super) async fn retry_with_backoff<T>(
    cancel_site: CancelSite,
    cancel: &Scope,
    mut one: impl AsyncFnMut(Option<&ProviderError>) -> Attempt<T>,
) -> Result<T, ProviderError> {
    let mut attempt = 0;
    let mut replaced = None;
    loop {
        attempt += 1;
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled(cancel_site));
        }
        let mut error = match one(replaced.as_ref()).await {
            Attempt::Done(value) => return Ok(value),
            Attempt::Failed(error @ ProviderError::Cancelled(_)) => return Err(error),
            Attempt::Failed(error) => error,
        };
        let Some((max_attempts, max_delay_ms, retry_after)) = budget(&error) else {
            return Err(error);
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
        replaced = Some(error);
    }
}

/// A scope has no waker; the 50 ms poll is the cancellation latency.
pub(super) async fn wait_for_cancel(cancel: &Scope) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Limit;
    use ral_core::process::DurableRoot;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build retry test runtime")
    }

    #[test]
    fn a_timeout_before_any_token_is_retried_then_surfaced() {
        let calls = std::cell::Cell::new(0u32);
        let out: Result<(), ProviderError> = runtime().block_on(retry_with_backoff(
            CancelSite::BeforeRequest,
            &DurableRoot::default().child(),
            async |_attempt| {
                calls.set(calls.get() + 1);
                Attempt::Failed(ProviderError::Transient {
                    cause: "model response headers timed out".into(),
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
            &DurableRoot::default().child(),
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
    fn each_attempt_sees_the_failure_it_replaces() {
        let seen = std::cell::RefCell::new(Vec::new());
        let _: Result<(), ProviderError> = runtime().block_on(retry_with_backoff(
            CancelSite::BeforeRequest,
            &DurableRoot::default().child(),
            async |replaced: Option<&ProviderError>| {
                seen.borrow_mut().push(replaced.map(ProviderError::summary));
                Attempt::Failed(ProviderError::local_transient(format!(
                    "failure {}",
                    seen.borrow().len()
                )))
            },
        ));
        let seen = seen.into_inner();
        assert_eq!(seen[0], None);
        assert_eq!(seen[1], Some("connection failed: failure 1".to_string()));
        assert_eq!(seen.len(), MAX_ATTEMPTS as usize);
    }

    #[test]
    fn cancellation_interrupts_backoff() {
        runtime().block_on(async {
            let cancel = DurableRoot::default().child();
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
