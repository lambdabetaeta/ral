//! The Codex backend's rate-limit readout: `GET /backend-api/wham/usage`, no
//! query parameters. Two opaque rolling windows, each a used-percentage; their
//! durations come from the server, never hardcoded here.

use crate::provider::allowance::{Allowance, Consumption};
use crate::provider::credential::{Credential, Roster};
use crate::provider::error::body_detail;
use crate::provider::identity::Account;
use crate::provider::models::blocking_runtime;
use crate::provider::oauth;
use ral_core::sync::LockExt;
use serde::Deserialize;
use std::time::Duration;

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

pub(in crate::provider::allowance) fn read(
    account: &Account,
    credential: &Credential,
    roster: &Roster,
) -> Result<Vec<Allowance>, String> {
    let Credential::OAuth(cell) = credential else {
        return Err(format!(
            "{} authenticates with an API key, not a ChatGPT login — the Codex \
             usage endpoint has nothing to read for it",
            roster.label(account)
        ));
    };
    let runtime = blocking_runtime("codex usage")?;
    runtime.block_on(async {
        oauth::refresh_cell_if_stale(cell)
            .await
            .map_err(|e| format!("refresh login for {}: {e}", roster.label(account)))?;
        let token = cell.lock_ignore_poison().clone();
        let request = oauth::request_headers(&token, "application/json")
            .into_iter()
            .fold(crate::provider::tls::client().get(USAGE_URL), |r, (k, v)| {
                r.header(k, v)
            });
        let response = request
            .send()
            .await
            .map_err(|e| format!("read usage for {}: {e}", roster.label(account)))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "read usage for {}: Codex backend returned HTTP {status}: {}",
                roster.label(account),
                body_detail(&body)
            ));
        }
        let body: UsageResponse = response
            .json()
            .await
            .map_err(|e| format!("parse usage for {}: {e}", roster.label(account)))?;
        // An empty reading is not an unmetered service: that is the table's
        // answer, never the wire's.
        usage_to_allowances(&body).ok_or_else(|| {
            format!(
                "the Codex backend reported no rate-limit windows for {} — \
                 is this login rationed by window at all?",
                roster.label(account)
            )
        })
    })
}

/// Upstream models `rate_limit` and each window as double-options: absent or
/// explicitly `null` both mean the same thing.
#[derive(Deserialize, Default)]
struct UsageResponse {
    #[serde(default)]
    rate_limit: Option<RateLimit>,
}

#[derive(Deserialize, Default)]
struct RateLimit {
    #[serde(default)]
    primary_window: Option<Window>,
    #[serde(default)]
    secondary_window: Option<Window>,
}

#[derive(Deserialize)]
struct Window {
    used_percent: f64,
    limit_window_seconds: Option<u64>,
    /// Absolute unix seconds. `reset_after_seconds` is discarded, as upstream
    /// itself does: a reading sitting on a channel cannot quietly age against
    /// an absolute figure the way it would against a relative one.
    reset_at: Option<u64>,
}

/// `None` when the payload disclosed no window at all — whether it omitted
/// `rate_limit`, sent it null, or sent one carrying neither window.
fn usage_to_allowances(body: &UsageResponse) -> Option<Vec<Allowance>> {
    let rate_limit = body.rate_limit.as_ref()?;
    let allowances: Vec<Allowance> = [&rate_limit.primary_window, &rate_limit.secondary_window]
        .into_iter()
        .flatten()
        .map(window_to_allowance)
        .collect();
    (!allowances.is_empty()).then_some(allowances)
}

fn window_to_allowance(window: &Window) -> Allowance {
    Allowance {
        window: window.limit_window_seconds.map(Duration::from_secs),
        used: Consumption::Fraction(window.used_percent / 100.0),
        resets_at: window.reset_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Option<Vec<Allowance>> {
        let wire: UsageResponse = serde_json::from_str(body).expect("valid fixture");
        usage_to_allowances(&wire)
    }

    /// Byte-for-byte the shape upstream sends: two windows, seconds
    /// throughout, an absolute `reset_at`.
    #[test]
    fn a_real_shaped_payload_yields_both_windows() {
        let allowances = parse(
            r#"{
                "rate_limit": {
                    "primary_window":   { "used_percent": 42, "limit_window_seconds": 18000,  "reset_after_seconds": 7200,   "reset_at": 1780000000 },
                    "secondary_window": { "used_percent": 11, "limit_window_seconds": 604800, "reset_after_seconds": 300000, "reset_at": 1780500000 }
                }
            }"#,
        );
        assert_eq!(
            allowances.expect("a payload carrying both windows discloses them"),
            vec![
                Allowance {
                    window: Some(Duration::from_hours(5)),
                    used: Consumption::Fraction(0.42),
                    resets_at: Some(1_780_000_000),
                },
                Allowance {
                    window: Some(Duration::from_hours(168)),
                    used: Consumption::Fraction(0.11),
                    resets_at: Some(1_780_500_000),
                },
            ]
        );
    }

    /// A metered login that disclosed nothing this time is not an unmetered
    /// one: only the table decides that, so the reading refuses rather than
    /// reporting an empty ration.
    #[test]
    fn a_payload_with_no_windows_discloses_nothing() {
        assert_eq!(parse(r#"{"rate_limit": null}"#), None);
        assert_eq!(parse("{}"), None);
        assert_eq!(parse(r#"{"rate_limit": {}}"#), None);
    }
}
