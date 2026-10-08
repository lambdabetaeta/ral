//! Device-code login: the user types a code into a verification page while
//! exarch polls. Unlike `browser`, the PKCE verifier arrives in the response.

use super::{CLIENT_ID, ISSUER, LoginPhase};
use serde::Deserialize;
use serde::Deserializer;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::time::Instant;

/// The poll deadline. Enforced here — the usercode response carries no
/// `expires_in` — and `browser`'s callback wait is in step with it.
const MAX_WAIT: Duration = Duration::from_mins(15);

/// `MAX_WAIT` as the sign-in messages state it, so the two cannot drift.
fn max_wait_label() -> String {
    format!("{} minutes", MAX_WAIT.as_secs() / 60)
}

#[derive(Deserialize)]
// `user_code` repeats the struct name because that is the wire field name.
#[allow(clippy::struct_field_names)]
struct UserCode {
    device_auth_id: String,
    #[serde(alias = "usercode")]
    user_code: String,
    #[serde(default = "default_interval", deserialize_with = "interval_secs")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// The server sends `interval` sometimes as a number, sometimes as a string.
fn interval_secs<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Interval {
        Num(u64),
        Str(String),
    }
    match Interval::deserialize(deserializer)? {
        Interval::Num(n) => Ok(n),
        Interval::Str(s) => s.trim().parse().map_err(serde::de::Error::custom),
    }
}

#[derive(Deserialize)]
struct PollSuccess {
    authorization_code: String,
    code_verifier: String,
}

/// Drive the device-code flow to completion and return the issued tokens.
pub(super) async fn run(
    client: &reqwest::Client,
    on_phase: impl Fn(LoginPhase),
    cancel: &Arc<AtomicBool>,
) -> Result<super::RawTokens, String> {
    let code = request_user_code(client).await?;

    on_phase(LoginPhase::AwaitingDevice {
        user_code: code.user_code.clone(),
        url: format!("{ISSUER}/codex/device"),
        expires_in: max_wait_label(),
    });

    let poll = poll_for_code(client, &code, cancel).await?;
    on_phase(LoginPhase::ExchangingCode);
    let redirect_uri = format!("{ISSUER}/deviceauth/callback");
    super::exchange_code(
        client,
        &redirect_uri,
        &poll.authorization_code,
        &poll.code_verifier,
    )
    .await
}

async fn request_user_code(client: &reqwest::Client) -> Result<UserCode, String> {
    let resp = client
        .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;
    super::json_or_error(resp, "device code request").await
}

/// Poll until the user authorises or `MAX_WAIT` elapses. `cancel` is read
/// between requests, so an in-flight one still runs out the client's timeout.
async fn poll_for_code(
    client: &reqwest::Client,
    code: &UserCode,
    cancel: &Arc<AtomicBool>,
) -> Result<PollSuccess, String> {
    let url = format!("{ISSUER}/api/accounts/deviceauth/token");
    let start = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("sign-in cancelled".to_string());
        }
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "device_auth_id": code.device_auth_id,
                "user_code": code.user_code,
            }))
            .send()
            .await
            .map_err(|e| format!("device token poll failed: {e}"))?;

        // 403/404 is this endpoint's "not yet"; every other status is
        // `json_or_error`'s to judge.
        if matches!(resp.status().as_u16(), 403 | 404) {
            if start.elapsed() >= MAX_WAIT {
                return Err(format!(
                    "device sign-in timed out after {}",
                    max_wait_label()
                ));
            }
            tokio::time::sleep(Duration::from_secs(code.interval)).await;
            continue;
        }

        return super::json_or_error(resp, "device token poll").await;
    }
}
