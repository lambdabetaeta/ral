//! Cancellation-aware retry policy and idle budgets for the attempt loops in
//! `stream.rs`, keyed on the variant `error.rs` classified.

use super::ProviderError;
use super::tls::STREAM_IDLE_TIMEOUT;
use crate::agent::cancel;
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

/// The first attempt keeps the full [`STREAM_IDLE_TIMEOUT`]; retries take the
/// shorter one, spending the attempt budget rather than the clock.
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

/// The server's own `Retry-After` wins when present, capped like the
/// exponential fallback so one provider value cannot stall the loop; the shift
/// cap only guards `1u64 << shift` against overflow.
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
    Failed(ProviderError),
}

/// Drive `one` through the shared retry policy: only `Transient` and
/// `RateLimited` are retried, each against its own budget, and cancellation
/// races the backoff sleep, so a cancel mid-delay never waits it out.
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

/// [`cancel::Token`] is a bare atomic with no waker, so a cancel is noticed
/// only on the next poll — the 50ms is that latency.
pub(super) async fn wait_for_cancel(cancel: &cancel::Token) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Only `Transient` carries an attempt count; the rest pass through untouched.
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

    /// From the wire error to the clock: a provider asking for ten minutes is
    /// honoured only up to the cap, so a 429 costs the session minutes, not an
    /// hour indistinguishable from a hang.
    #[test]
    fn rate_limited_429_spends_its_budget_under_the_delay_cap() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("600"),
        );
        let error = ProviderError::from_genai(
            &genai::Error::WebModelCall {
                model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::Anthropic, "m"),
                webc_error: genai::webc::Error::ResponseFailedStatus {
                    status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                    body: String::new(),
                    headers: Box::new(headers),
                },
            },
            "m",
        );
        assert!(matches!(
            error,
            ProviderError::RateLimited {
                retry_after: Some(_),
                ..
            }
        ));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .expect("build paused retry test runtime");
        runtime.block_on(async {
            let calls = std::cell::Cell::new(0u32);
            let start = tokio::time::Instant::now();
            let out: Result<(), ProviderError> =
                retry_with_backoff("test", &cancel::Token::new(), async |_attempt| {
                    calls.set(calls.get() + 1);
                    Attempt::Failed(error.clone())
                })
                .await;
            assert!(matches!(out, Err(ProviderError::RateLimited { .. })));
            assert_eq!(calls.get(), RATE_LIMIT_MAX_ATTEMPTS);
            let elapsed = start.elapsed();
            assert!(
                elapsed >= Duration::from_secs(140) && elapsed < Duration::from_secs(200),
                "five waits capped at {RATE_LIMIT_MAX_DELAY_MS}ms, got {elapsed:?}"
            );
        });
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
