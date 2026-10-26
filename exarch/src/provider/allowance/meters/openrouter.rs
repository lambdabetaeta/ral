//! `OpenRouter`'s effective spending budget: `GET /api/v1/key`.

use crate::provider::allowance::{Allowance, BudgetKind, BudgetReset, Consumption, Scope, Unit};
use crate::provider::credential::{Credential, Roster};
use crate::provider::error::body_detail;
use crate::provider::identity::Account;
use crate::provider::models::blocking_runtime;
use jiff::Timestamp;
use serde::Deserialize;

const KEY_URL: &str = "https://openrouter.ai/api/v1/key";

pub(in crate::provider::allowance) fn read(
    account: &Account,
    credential: &Credential,
    roster: &Roster,
) -> Result<Vec<Allowance>, String> {
    let Credential::ApiKey(key) = credential else {
        return Err(format!(
            "{} authenticates with a login, not an API key: OpenRouter's \
             budget endpoint has nothing to read for it",
            roster.label(account)
        ));
    };
    let key = key.clone();
    let runtime = blocking_runtime("openrouter budget")?;
    runtime.block_on(async move {
        let response = crate::provider::tls::client()
            .get(KEY_URL)
            .bearer_auth(&key)
            .send()
            .await
            .map_err(|e| format!("read budget for {}: {e}", roster.label(account)))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "read budget for {}: OpenRouter returned HTTP {status}: {}",
                roster.label(account),
                body_detail(&body)
            ));
        }
        let body: KeyResponse = response
            .json()
            .await
            .map_err(|e| format!("parse budget for {}: {e}", roster.label(account)))?;
        key_to_allowances(&body)
            .map_err(|e| format!("read budget for {}: {e}", roster.label(account)))
    })
}

#[derive(Deserialize)]
struct KeyResponse {
    data: KeyData,
}

#[derive(Deserialize)]
struct KeyData {
    usage: f64,
    limit: Option<f64>,
    limit_remaining: Option<f64>,
    limit_reset: Option<BudgetReset>,
    #[serde(default)]
    include_byok_in_limit: bool,
    byok_usage: Option<f64>,
    effective_budget: Option<KeyBudget>,
}

#[derive(Deserialize)]
struct KeyBudget {
    scope: BudgetKind,
    limit_usd: f64,
    spend_usd: f64,
    reset_interval: Option<BudgetReset>,
    resets_at: Option<Timestamp>,
}

fn key_to_allowances(body: &KeyResponse) -> Result<Vec<Allowance>, String> {
    let data = &body.data;
    let (kind, reset, used, limit, resets_at) = if let Some(budget) = &data.effective_budget {
        (
            budget.scope,
            budget.reset_interval,
            budget.spend_usd,
            Some(budget.limit_usd),
            budget.resets_at,
        )
    } else {
        let used = match (data.limit, data.limit_remaining) {
            (Some(limit), Some(remaining)) => limit - remaining,
            (Some(_), None) if data.limit_reset.is_some() => {
                return Err(
                    "OpenRouter disclosed a resetting key limit without its remaining amount; \
                     lifetime usage cannot measure this budget"
                        .into(),
                );
            }
            _ => {
                let byok = if data.include_byok_in_limit {
                    data.byok_usage.ok_or(
                        "OpenRouter includes BYOK spend in this budget but disclosed no BYOK usage",
                    )?
                } else {
                    0.0
                };
                data.usage + byok
            }
        };
        (
            BudgetKind::Key,
            data.limit.and(data.limit_reset),
            used,
            data.limit,
            None,
        )
    };
    Ok(vec![Allowance {
        scope: Scope::Budget { kind, reset },
        used: Consumption::Counted {
            used: dollars_to_cents(used),
            limit: limit.map(dollars_to_cents),
            unit: Unit::Dollars,
        },
        resets_at,
    }])
}

/// `Unit::Dollars` counts cents; overspending is retained, never a negative spend.
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

    fn parse(body: &str) -> Result<Allowance, String> {
        let wire: KeyResponse = serde_json::from_str(body).expect("valid fixture");
        Ok(key_to_allowances(&wire)?.remove(0))
    }

    #[test]
    fn a_monthly_key_uses_remaining_not_lifetime_spend() {
        let a = parse(
            r#"{"data": {
            "usage": 160, "limit": 100, "limit_remaining": 80, "limit_reset": "monthly"
        }}"#,
        )
        .unwrap();
        assert_eq!(a.percent(), Some(20));
        assert_eq!(a.figure_text(), "$20.00 used / $100.00 · $80.00 remaining");
        assert_eq!(a.label(), "key budget · monthly");
        assert!(a.scope.renews());
    }

    #[test]
    fn the_effective_workspace_budget_overrides_the_keys_larger_limit() {
        let a = parse(
            r#"{"data": {
            "usage": 160, "limit": 100, "limit_remaining": 80, "limit_reset": "monthly",
            "effective_budget": {
                "scope": "workspace", "limit_usd": 10, "spend_usd": 9.5,
                "remaining_usd": 0.5, "reset_interval": "weekly",
                "resets_at": "2026-11-02T00:00:00Z"
            }
        }}"#,
        )
        .unwrap();
        assert_eq!(a.percent(), Some(95));
        assert_eq!(a.figure_text(), "$9.50 used / $10.00 · $0.50 remaining");
        let now: Timestamp = "2026-11-01T00:00:00Z".parse().unwrap();
        assert_eq!(
            a.label_at(now),
            "workspace budget · weekly · resets in 1d 00h"
        );
    }

    #[test]
    fn a_guardrail_can_limit_an_otherwise_uncapped_key() {
        let a = parse(
            r#"{"data": {
            "usage": 160, "limit": null,
            "effective_budget": {
                "scope": "member", "limit_usd": 50, "spend_usd": 40,
                "remaining_usd": 10, "reset_interval": "daily", "resets_at": null
            }
        }}"#,
        )
        .unwrap();
        assert_eq!(a.percent(), Some(80));
        assert_eq!(a.label(), "member guardrail budget · daily");
    }

    #[test]
    fn an_uncapped_key_does_not_claim_an_unlimited_account_balance() {
        for limit in ["", ", \"limit\": null"] {
            let a = parse(&format!(r#"{{"data": {{"usage": 12.4{limit}}}}}"#)).unwrap();
            assert_eq!(a.percent(), None);
            assert_eq!(a.label(), "key budget · lifetime");
            assert_eq!(
                a.figure_text(),
                "$12.40 used · no published cap · account balance unknown"
            );
        }
    }

    #[test]
    fn a_zero_cap_is_spent_even_without_usage() {
        let a = parse(r#"{"data": {"usage": 0, "limit": 0, "limit_remaining": 0}}"#).unwrap();
        assert_eq!(a.percent(), Some(100));
        assert_eq!(a.figure_text(), "$0.00 used / $0.00 · $0.00 remaining");
    }

    #[test]
    fn overspending_is_not_lost() {
        let a = parse(r#"{"data": {"usage": 160, "limit": 100, "limit_remaining": -5}}"#).unwrap();
        assert_eq!(a.percent(), Some(100));
        assert_eq!(a.figure_text(), "$105.00 used / $100.00 · $0.00 remaining");
    }

    #[test]
    fn a_resetting_budget_without_remaining_is_not_guessed() {
        let error = parse(r#"{"data": {"usage": 160, "limit": 100, "limit_reset": "monthly"}}"#)
            .unwrap_err();
        assert!(error.contains("lifetime usage cannot measure this budget"));
    }

    #[test]
    fn legacy_lifetime_limits_include_byok_only_when_enforced() {
        for (include, expected) in [(false, 20), (true, 30)] {
            let a = parse(&format!(
                r#"{{"data": {{
                "usage": 20, "limit": 100, "byok_usage": 10,
                "include_byok_in_limit": {include}
            }}}}"#
            ))
            .unwrap();
            assert_eq!(a.percent(), Some(expected));
            assert!(!a.scope.renews());
        }
    }

    #[test]
    fn corrected_readings_feed_the_shared_warning_latches() {
        let account = Account::built_in("openrouter");
        let rations = crate::provider::Rations::default();
        let a = parse(
            r#"{"data": {
            "usage": 160, "limit": 100, "limit_remaining": 80, "limit_reset": "monthly"
        }}"#,
        )
        .unwrap();
        rations.land(&account.id, vec![a]);
        assert_eq!(rations.climb(&account.id, &[50, 75, 90, 95]), []);
        let a = parse(
            r#"{"data": {
            "usage": 160, "limit": 100, "limit_remaining": 5, "limit_reset": "monthly"
        }}"#,
        )
        .unwrap();
        rations.land(&account.id, vec![a]);
        let climbed = rations.climb(&account.id, &[50, 75, 90, 95]);
        assert_eq!(climbed.len(), 1);
        assert_eq!(climbed[0].percent(), Some(95));
        assert_eq!(rations.climb(&account.id, &[50, 75, 90, 95]), []);
    }

    #[test]
    fn byok_is_not_added_twice_to_the_reported_remaining_amount() {
        let a = parse(
            r#"{"data": {
            "usage": 160, "limit": 100, "limit_remaining": 80,
            "include_byok_in_limit": true, "byok_usage": 50
        }}"#,
        )
        .unwrap();
        assert_eq!(a.percent(), Some(20));
    }
}
