//! Device-code login: the user opens a verification page and types a code
//! while exarch polls the token endpoint. The poll response carries the PKCE
//! verifier used in the final authorization-code exchange.

use super::{CLIENT_ID, ISSUER};
use serde::Deserialize;
use serde::Deserializer;
use std::time::Duration;
use std::time::Instant;

const MAX_WAIT: Duration = Duration::from_mins(15);

#[derive(Deserialize)]
// Field names are fixed by the device-authorization wire format (serde aliases).
#[allow(clippy::struct_field_names)]
struct UserCode {
    device_auth_id: String,
    #[serde(alias = "user_code", alias = "usercode")]
    user_code: String,
    #[serde(default = "default_interval", deserialize_with = "interval_secs")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// Accept the polling interval whether the server sends it as a number or a
/// string.
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
pub(super) async fn run(client: &reqwest::Client) -> Result<super::RawTokens, String> {
    let code = request_user_code(client).await?;

    eprintln!(
        "To sign in, open {ISSUER}/codex/device and enter this code (expires in 15 minutes):\n  {}",
        code.user_code
    );

    let poll = poll_for_code(client, &code).await?;
    let redirect_uri = format!("{ISSUER}/deviceauth/callback");
    super::exchange_code(
        client,
        &redirect_uri,
        &poll.authorization_code,
        &poll.code_verifier,
    )
    .await
}

/// Request a one-time user code and the device auth id that identifies it.
async fn request_user_code(client: &reqwest::Client) -> Result<UserCode, String> {
    let resp = client
        .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .map_err(|e| format!("device code request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("device code request failed ({status}): {body}"));
    }
    resp.json()
        .await
        .map_err(|e| format!("could not parse device code response: {e}"))
}

/// Poll the token endpoint until the user authorises the code or 15 minutes
/// elapse. A 403 or 404 means the user has not finished yet.
async fn poll_for_code(client: &reqwest::Client, code: &UserCode) -> Result<PollSuccess, String> {
    let url = format!("{ISSUER}/api/accounts/deviceauth/token");
    let start = Instant::now();
    loop {
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
        let status = resp.status();

        if status.is_success() {
            return resp
                .json()
                .await
                .map_err(|e| format!("could not parse device token response: {e}"));
        }

        if matches!(status.as_u16(), 403 | 404) {
            if start.elapsed() >= MAX_WAIT {
                return Err("device sign-in timed out after 15 minutes".to_string());
            }
            tokio::time::sleep(Duration::from_secs(code.interval)).await;
            continue;
        }

        let body = resp.text().await.unwrap_or_default();
        return Err(format!("device token poll failed ({status}): {body}"));
    }
}
