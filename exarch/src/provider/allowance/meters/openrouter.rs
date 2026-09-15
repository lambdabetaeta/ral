//! `OpenRouter`'s credit-balance readout: `GET /api/v1/key`.

use crate::provider::allowance::{Allowance, Consumption, Unit};
use crate::provider::credential::{Credential, Roster};
use crate::provider::error::body_detail;
use crate::provider::identity::Account;
use crate::provider::models::blocking_runtime;
use serde::Deserialize;

const KEY_URL: &str = "https://openrouter.ai/api/v1/key";

pub(in crate::provider::allowance) fn read(
    account: &Account,
    credential: &Credential,
    roster: &Roster,
) -> Result<Vec<Allowance>, String> {
    let Credential::ApiKey(key) = credential else {
        return Err(format!(
            "{} authenticates with a ChatGPT login, not an API key — OpenRouter's \
             credit endpoint has nothing to read for it",
            roster.label(account)
        ));
    };
    let key = key.clone();
    let runtime = blocking_runtime("openrouter credits")?;
    runtime.block_on(async move {
        let response = crate::provider::tls::client()
            .get(KEY_URL)
            .bearer_auth(&key)
            .send()
            .await
            .map_err(|e| format!("read credits for {}: {e}", roster.label(account)))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "read credits for {}: OpenRouter returned HTTP {status}: {}",
                roster.label(account),
                body_detail(&body)
            ));
        }
        let body: KeyResponse = response
            .json()
            .await
            .map_err(|e| format!("parse credits for {}: {e}", roster.label(account)))?;
        Ok(key_to_allowances(&body))
    })
}

#[derive(Deserialize)]
struct KeyResponse {
    data: KeyData,
}

#[derive(Deserialize)]
struct KeyData {
    usage: f64,
    /// A null or absent cap is an uncapped balance, a real state distinct
    /// from a cap of `0`.
    #[serde(default)]
    limit: Option<f64>,
}

fn key_to_allowances(body: &KeyResponse) -> Vec<Allowance> {
    vec![Allowance {
        window: None,
        used: Consumption::Counted {
            used: dollars_to_cents(body.data.usage),
            limit: body.data.limit.map(dollars_to_cents),
            unit: Unit::Dollars,
        },
        resets_at: None,
    }]
}

/// `Unit::Dollars` counts cents, so `$12.40` becomes `1240`. Never negative:
/// a balance is a spend, not a credit.
#[allow(
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    reason = "clamped to 0.0.. above; a dollar figure this large would already be a vendor error"
)]
fn dollars_to_cents(dollars: f64) -> u64 {
    (dollars.max(0.0) * 100.0).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<Allowance> {
        let wire: KeyResponse = serde_json::from_str(body).expect("valid fixture");
        key_to_allowances(&wire)
    }

    /// Byte-for-byte the shape a real `/api/v1/key` response takes, extra
    /// fields (`label`, `is_free_tier`, ...) ignored.
    #[test]
    fn a_real_shaped_payload_yields_one_allowance() {
        let allowances = parse(
            r#"{
                "data": {
                    "label": "sk-or-v1-...abcd",
                    "usage": 12.4,
                    "is_free_tier": false,
                    "limit": 50.0,
                    "limit_remaining": 37.6,
                    "rate_limit": { "requests": 200, "interval": "10s" }
                }
            }"#,
        );
        assert_eq!(
            allowances,
            vec![Allowance {
                window: None,
                used: Consumption::Counted {
                    used: 1240,
                    limit: Some(5000),
                    unit: Unit::Dollars,
                },
                resets_at: None,
            }]
        );
    }

    #[test]
    fn a_null_limit_is_an_uncapped_balance() {
        let allowances = parse(r#"{"data": {"usage": 3.0, "limit": null}}"#);
        assert_eq!(
            allowances,
            vec![Allowance {
                window: None,
                used: Consumption::Counted {
                    used: 300,
                    limit: None,
                    unit: Unit::Dollars,
                },
                resets_at: None,
            }]
        );
    }
}
