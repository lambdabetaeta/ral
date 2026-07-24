//! Cancellation-aware retry policy and idle budgets.

use super::ProviderError;
use super::tls::STREAM_IDLE_TIMEOUT;
use crate::agent::cancel;
use std::time::Duration;

/// Retry budget for transient stream and network failures.
pub(crate) const MAX_ATTEMPTS: u32 = 3;
/// Rate limits get a longer leash than a generic transient: a 429 means
/// the provider is telling us to wait, not that the request is broken, so
/// giving up early wastes a turn the provider would have served.
const RATE_LIMIT_MAX_ATTEMPTS: u32 = 6;
const BASE_DELAY_MS: u64 = 750;
const MAX_DELAY_MS: u64 = 8_000;
/// Providers commonly ask for tens of seconds via `Retry-After` on a 429;
/// this ceiling is high enough to honour that wait rather than abandon
/// the retry loop first.
const RATE_LIMIT_MAX_DELAY_MS: u64 = 30_000;
const RETRY_IDLE_TIMEOUT: Duration = Duration::from_mins(1);

/// The first attempt gets the full [`STREAM_IDLE_TIMEOUT`] — a slow
/// time-to-first-token should not be mistaken for a dead connection.
/// Every retry after that uses the shorter [`RETRY_IDLE_TIMEOUT`], so a
/// run stuck hitting dead connections fails out of the retry budget
/// instead of waiting out the long timeout on each attempt.
pub(super) fn idle_timeout(attempt: u32) -> Duration {
    if attempt <= 1 {
        STREAM_IDLE_TIMEOUT
    } else {
        RETRY_IDLE_TIMEOUT
    }
}

fn retry_limits(error: &ProviderError) -> (u32, u64) {
    match error {
        ProviderError::RateLimited { .. } => (RATE_LIMIT_MAX_ATTEMPTS, RATE_LIMIT_MAX_DELAY_MS),
        _ => (MAX_ATTEMPTS, MAX_DELAY_MS),
    }
}

/// Honour the server's own `Retry-After` wait when present, capped at
/// `max_delay_ms` so one provider-supplied value can't stall the whole
/// loop; otherwise back off exponentially from `BASE_DELAY_MS`, same cap.
/// The shift is capped at 16 defensively — well past any real attempt
/// count the retry budgets above allow — so `1u64 << shift` can never
/// overflow.
async fn backoff_sleep(attempt: u32, retry_after: Option<Duration>, max_delay_ms: u64) {
    if let Some(delay) = retry_after {
        tokio::time::sleep(delay.min(Duration::from_millis(max_delay_ms))).await;
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
    /// An error the driver may retry.
    Failed(ProviderError),
}

/// Drive `one` through the shared retry/backoff policy.
///
/// Only [`ProviderError::Transient`] and [`ProviderError::RateLimited`]
/// are retried, each against its own budget from [`retry_limits`]; every
/// other variant returns immediately, including a `Cancelled` that `one`
/// raises itself.  Cancellation is checked before each attempt and raced
/// against the backoff sleep via a `biased` `select!`, so a cancel signal
/// that lands mid-backoff is never made to wait out the delay.
/// `cancel_site` labels this call's own `Cancelled` for that race and the
/// pre-attempt check.
pub(super) async fn retry_with_backoff<T>(
    cancel_site: &'static str,
    cancel: &cancel::Token,
    mut one: impl AsyncFnMut(u32) -> Attempt<T>,
) -> Result<T, ProviderError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        if cancel.is_cancelled() {
            return Err(ProviderError::Cancelled(cancel_site));
        }
        let error = match one(attempt).await {
            Attempt::Done(value) => return Ok(value),
            Attempt::Failed(error @ ProviderError::Cancelled(_)) => return Err(error),
            Attempt::Failed(error) => error,
        };
        if !matches!(
            error,
            ProviderError::Transient { .. } | ProviderError::RateLimited { .. }
        ) {
            return Err(stamp_attempts(error, attempt));
        }
        let (max_attempts, max_delay_ms) = retry_limits(&error);
        if attempt >= max_attempts {
            return Err(stamp_attempts(error, attempt));
        }
        let retry_after = match &error {
            ProviderError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        };
        tokio::select! {
            biased;
            () = wait_for_cancel(cancel) => {
                return Err(ProviderError::Cancelled(cancel_site));
            }
            () = backoff_sleep(attempt, retry_after, max_delay_ms) => {}
        }
    }
}

/// Resolves once `cancel` is raised.  [`cancel::Token`] is a bare atomic
/// flag with no waker to notify, so this polls at a coarse 50ms grain —
/// fine as the losing arm of the `biased select!`s that race it against a
/// request, a stream read, or a backoff sleep.
pub(super) async fn wait_for_cancel(cancel: &cancel::Token) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Overwrite the final attempt count on a `Transient` failure.
/// [`ProviderError::RateLimited`] carries no such field, so it — and
/// everything else — passes through unchanged.
fn stamp_attempts(error: ProviderError, attempts: u32) -> ProviderError {
    match error {
        ProviderError::Transient { cause, body, .. } => ProviderError::Transient {
            cause,
            attempts,
            body,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build retry test runtime")
    }

    #[test]
    fn rate_limit_gets_larger_budget_than_transient() {
        let rate_limit = ProviderError::RateLimited {
            retry_after: None,
            cause: "429".into(),
            body: None,
        };
        let transient = ProviderError::Transient {
            cause: "boom".into(),
            attempts: 1,
            body: None,
        };
        let (rate_attempts, rate_ceiling) = retry_limits(&rate_limit);
        let (transient_attempts, transient_ceiling) = retry_limits(&transient);
        assert_eq!(rate_attempts, RATE_LIMIT_MAX_ATTEMPTS);
        assert_eq!(rate_ceiling, RATE_LIMIT_MAX_DELAY_MS);
        assert_eq!(transient_attempts, MAX_ATTEMPTS);
        assert_eq!(transient_ceiling, MAX_DELAY_MS);
        assert!(rate_attempts > transient_attempts);
        assert!(rate_ceiling > transient_ceiling);
    }

    #[test]
    fn idle_timeout_before_token_is_retried_then_surfaced() {
        let calls = std::cell::Cell::new(0u32);
        let out: Result<(), ProviderError> = runtime().block_on(retry_with_backoff(
            "test",
            &cancel::Token::new(),
            async |_attempt| {
                calls.set(calls.get() + 1);
                Attempt::Failed(ProviderError::Transient {
                    cause: "stream idle: no response within timeout".into(),
                    attempts: 1,
                    body: None,
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
                cancel_during_wait.cancel(ral_core::process::CancelCause::Interrupt);
            });
            let out: Result<(), ProviderError> =
                retry_with_backoff("during retry backoff", &cancel, async |_| {
                    Attempt::Failed(ProviderError::Transient {
                        cause: "retry me".into(),
                        attempts: 1,
                        body: None,
                    })
                })
                .await;
            assert!(matches!(
                out,
                Err(ProviderError::Cancelled("during retry backoff"))
            ));
        });
    }
}
